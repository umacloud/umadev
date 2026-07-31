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
    /// The designer's design system **CONFORMS**, not merely exists
    /// (`VerifyKind::DesignSystemConform`, UD-CODE-007 / spec §3.7) — the STRONGER
    /// contract. [`Self::DesignTokensPresent`] passes on `:root{--color-bg:#000}`;
    /// this one demands a real system: >= 6 color roles each with a paired `on-`
    /// foreground, a >= 4-step type scale at ratio >= 1.125, a 4pt spacing scale, a
    /// radius scale, >= 2 durations + >= 1 easing; every declared (surface,
    /// on-surface) pair MEASURED against WCAG; the UI source actually drawing from
    /// the token set; and no AI-purple brand hue unless the user asked for one.
    /// Fail-open: no token file → a neutral skip (today's behaviour).
    DesignTokensConform,
    /// A review step is accepted by its reviewing seat (no blocking verdict).
    ReviewClean,
    /// No machine criterion — accepted when its work turn settles. The weakest
    /// criterion; used when the brain names nothing checkable. (Still bounded by the
    /// surrounding loop; never a free pass to ship.)
    TurnSettled,
}

impl AcceptanceSpec {
    /// Human-readable mechanical bar used in a step's rework directive.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::SourcePresent => "real source files exist on disk",
            Self::BuildTest => "the project's build/test passes",
            Self::Contract => "the frontend↔backend API contract holds",
            Self::DesignTokensPresent => {
                "the design-tokens.{json,css} design system exists on disk"
            }
            Self::DesignTokensConform => {
                "the design system CONFORMS: >=6 color roles each with a paired `on-` foreground, \
                 a >=4-step type scale, a 4pt spacing scale, a radius scale, motion tokens; every \
                 (surface, on-surface) pair measured against WCAG; the UI drawing from the tokens; \
                 no AI-purple brand hue"
            }
            Self::ReviewClean => "the review team raises no blocking issue",
            Self::TurnSettled => "the work turn completes",
        }
    }

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
            "design-tokens-conform"
            | "design-system-conform"
            | "design-conformance"
            | "tokens-conform" => Self::DesignTokensConform,
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
    /// **RED→GREEN**: ONE named test that must FAIL at this step's PRE-state and PASS
    /// at head — the falsifiable form of "test-first".
    ///
    /// [`Self::TestPasses`] can be satisfied by a test written AFTER the code it
    /// "checks", which proves nothing: a test that never failed has never demonstrated
    /// that it can detect the absence of the behaviour. This contract closes that hole
    /// mechanically. UmaDev rewinds the workspace to the step's pre-state in a
    /// SCOPED, REVERSIBLE way ([`crate::checkpoint::begin_temp_rewind`]), runs the ONE
    /// named test there (it MUST fail — or not exist, which is the same fact), restores
    /// head, and runs it again (it MUST pass). A test that was ALREADY GREEN before the
    /// step is a rejected step, with the diagnosis stated plainly.
    ///
    /// Opt-in per step, never global, and fail-open at every inconclusive edge (no
    /// rewind available, no test runner, a timeout) — those degrade to
    /// [`Self::TestPasses`] semantics rather than blocking on our own inability to
    /// verify.
    TestFailsThenPasses {
        /// The single test id/name to run in isolation (a test-name filter the
        /// project's own runner understands — NOT the whole suite).
        test: String,
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
            Self::TestFailsThenPasses { test } => {
                format!("test \"{test}\" FAILS before this step and PASSES after it (red→green)")
            }
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
            // RED→GREEN. A name is MANDATORY: the check runs ONE named test at two
            // tree states, so "the suite" is not expressible here. A missing name is a
            // retained GAP (M6), never a silent drop to the coarse default.
            "test-fails-then-passes" | "red-green" | "test-red-green" | "fails-then-passes" => {
                let test = {
                    let t = str_field("test");
                    if t.is_empty() {
                        str_field("name")
                    } else {
                        t
                    }
                };
                if test.is_empty() {
                    Some(Self::malformed(
                        "test-fails-then-passes",
                        "requires a non-empty `test` naming ONE test to run",
                    ))
                } else {
                    Some(Self::TestFailsThenPasses { test })
                }
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

/// Whether a step is the designer's **design-tokens deliverable** BUILD step — a
/// UIUX-seated Build step whose acceptance is [`AcceptanceSpec::DesignTokensPresent`]
/// (the typed discriminator; there is no `EvidenceContract` tokens variant). This is
/// CODE-PHASE PREP (write real `design-tokens.{json,css}` the frontend imports), NOT
/// the UIUX *doc* authoring step: it never anchors the doc-first chain, never gains
/// the uiux-doc evidence floor, and never counts toward (or triggers) the
/// `docs_confirm` gate family — it runs AFTER the docs are confirmed.
#[must_use]
pub fn is_design_tokens_step(s: &PlanStep) -> bool {
    s.kind == StepKind::Build
        && s.seat == crate::critics::Seat::UiuxDesigner
        && matches!(
            s.acceptance,
            AcceptanceSpec::DesignTokensPresent | AcceptanceSpec::DesignTokensConform
        )
}

/// The step id of the designer's **visual-direction** step (UD-CODE-007f) — the
/// step that must land BEFORE any token.
pub const DESIGN_DIRECTION_STEP_ID: &str = "umadev-phase-design-direction";

/// Whether `s` is the designer's **visual-direction** step. Like the tokens step,
/// it is UIUX-seated but is **code-phase prep, not doc authoring**: it refines the
/// UIUX doc that the doc-family step already produced, so it must never be adopted
/// as the UIUX doc anchor and must never re-fire the docs gate.
#[must_use]
pub fn is_design_direction_step(s: &PlanStep) -> bool {
    s.kind == StepKind::Build
        && s.seat == crate::critics::Seat::UiuxDesigner
        && s.id == DESIGN_DIRECTION_STEP_ID
}

/// Whether `s` is a designer step that is **code-phase PREP** (the visual
/// direction, or the design-tokens deliverable) rather than DOC AUTHORING. Both
/// are excluded from the docs family and from the docs-gate trigger.
#[must_use]
pub fn is_design_prep_step(s: &PlanStep) -> bool {
    is_design_tokens_step(s) || is_design_direction_step(s)
}

/// A documentation-family build step that can open the docs confirmation gate.
pub(crate) fn is_core_doc_build_step(step: &PlanStep) -> bool {
    step.kind == StepKind::Build
        && matches!(
            step.seat,
            Seat::ProductManager | Seat::Architect | Seat::UiuxDesigner
        )
        && !is_design_prep_step(step)
}

/// Whether implementation is now running after every core documentation step.
pub(crate) fn is_post_docs_implementation_step(step: &PlanStep, plan: &Plan) -> bool {
    step.kind == StepKind::Build
        && !is_core_doc_build_step(step)
        && plan.steps.iter().any(is_core_doc_build_step)
        && plan
            .steps
            .iter()
            .filter(|candidate| is_core_doc_build_step(candidate))
            .all(|candidate| candidate.status == StepStatus::Done)
}

/// Whether an existing QA BUILD step can safely run **before any code exists** —
/// the adoption bar for treating a brain-authored QA step as THE test-authoring
/// (test-first) step that frontend/backend code is wired BEHIND. Its acceptance /
/// evidence must only demand *authored artifacts* (files on disk); a bar that needs
/// a GREEN suite/build or a live route ([`AcceptanceSpec::BuildTest`],
/// [`EvidenceContract::TestPasses`] / `TestFailsThenPasses` / `BuildClean` /
/// `RouteResponds` / `ContractMatches`, or a held [`EvidenceContract::Malformed`] gap)
/// can never pass while the code it tests is unbuilt — wiring code behind such a step
/// would deadlock the plan, so it is NOT adopted (the skeleton inserts its own safe
/// test-authoring step instead).
///
/// The RED→GREEN contract in particular belongs on the step that turns a test GREEN
/// (the implementation step), NEVER on the step that AUTHORS it: an authored test is
/// expected to be red at head until the code lands. Pure.
fn qa_step_precode_runnable(s: &PlanStep) -> bool {
    let acceptance_ok = matches!(
        s.acceptance,
        AcceptanceSpec::SourcePresent | AcceptanceSpec::TurnSettled
    );
    let evidence_ok = s.evidence.iter().all(|e| {
        matches!(
            e,
            EvidenceContract::SourcePresent
                | EvidenceContract::FileExists { .. }
                | EvidenceContract::FileContains { .. }
        )
    });
    acceptance_ok && evidence_ok
}

/// The repo-relative directory the skeleton-inserted QA test-authoring step names as
/// its deliverable location (checked via [`EvidenceContract::FileExists`] — "the
/// authored tests exist on disk", NOT "the suite is green": tests are written BEFORE
/// the code they check, so they are EXPECTED to fail until the code lands).
const TEST_AUTHORING_DIR: &str = "tests";

/// The **declared file surface** of one plan step — the repo-relative paths the step
/// says it will bring into existence (`create`) or edit (`modify`).
///
/// This is the DUAL of requirement coverage. [`crate::coverage`] answers "which
/// declared requirement has no step?" (UNDER-building); a declared surface lets
/// [`crate::scope_creep`] answer the opposite question — "which CHANGE belongs to no
/// step?" (OVER-building). Without it, a run that quietly adds an unplanned
/// dependency, an unplanned source file, or an unplanned public route lands those
/// changes with nobody having asked for them and nothing having reviewed them.
///
/// Entries are repo-relative, `/`-separated. A trailing `/` (or a bare directory
/// name that exists as a directory) claims the whole subtree, so a step can claim
/// `src/api/` without enumerating every file it will write there.
///
/// Required for every mutating [`StepKind::Build`] step. Exact file evidence is
/// losslessly projected into this surface during normalisation; a remaining empty
/// surface is an under-specified execution contract and is repaired by the planner
/// before a writer may start. Older persisted plans still deserialize through the
/// default, but resume preflight reports the missing contract instead of silently
/// disabling scope enforcement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFiles {
    /// Repo-relative paths (or directory prefixes) this step will CREATE.
    #[serde(default)]
    pub create: Vec<String>,
    /// Repo-relative paths (or directory prefixes) this step will MODIFY.
    #[serde(default)]
    pub modify: Vec<String>,
}

impl StepFiles {
    /// Whether the step declared any surface at all. An all-empty declaration is the
    /// same as no declaration (the scope check stays silent).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.create.is_empty() && self.modify.is_empty()
    }

    /// Every declared path (create + modify), in declaration order.
    pub fn all(&self) -> impl Iterator<Item = &str> {
        self.create
            .iter()
            .chain(self.modify.iter())
            .map(String::as_str)
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
    /// The step's DECLARED FILE SURFACE — what it says it will create / modify (see
    /// [`StepFiles`]). Read by the scope-creep floor ([`crate::scope_creep`]) to
    /// decide whether a change in the working tree belongs to ANY step. The serde
    /// default exists only for old `plan.json` compatibility; execution preflight
    /// rejects a mutating step that still has no surface after evidence inference.
    #[serde(default)]
    pub files: StepFiles,
    /// Lifecycle status (persisted, so the plan resumes).
    pub status: StepStatus,
}

impl PlanStep {
    /// Typed evidence summary when present, otherwise the coarse acceptance bar.
    pub(crate) fn criterion_label(&self) -> String {
        if self.evidence.is_empty() {
            self.acceptance.label().to_string()
        } else {
            self.evidence
                .iter()
                .map(EvidenceContract::label)
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

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

    /// Re-open every step (and its transitive downstream) whose seat READS an
    /// artifact that is now STALE - the blackboard staleness invalidation the
    /// versioning store enables (docs/AGENT_TEAM_INTERACTION_DESIGN.md item C). A
    /// step's consumed artifacts are its seat's [`crate::critics::SeatCard::reads`];
    /// if any is in `stale`, the step - and everything transitively depending on it -
    /// flips back to [`StepStatus::Pending`] so the director re-derives it against the
    /// changed upstream instead of trusting a now-poisoned result. Deterministic +
    /// pure; the director computes `stale` (via `critics::stale_artifacts`) and calls
    /// this on re-plan. Returns re-opened ids (sorted). Fail-open: empty re-opens none.
    pub fn invalidate_stale(&mut self, stale: &[crate::critics::ArtifactKind]) -> Vec<String> {
        if stale.is_empty() {
            return Vec::new();
        }
        let mut stale_ids: HashSet<String> = self
            .steps
            .iter()
            .filter(|s| s.seat.card().reads.iter().any(|r| stale.contains(r)))
            .map(|s| s.id.clone())
            .collect();
        loop {
            let before = stale_ids.len();
            for s in &self.steps {
                if !stale_ids.contains(&s.id) && s.depends_on.iter().any(|d| stale_ids.contains(d))
                {
                    stale_ids.insert(s.id.clone());
                }
            }
            if stale_ids.len() == before {
                break;
            }
        }
        let mut reopened = Vec::new();
        for s in &mut self.steps {
            if stale_ids.contains(&s.id) && s.status != StepStatus::Pending {
                s.status = StepStatus::Pending;
                reopened.push(s.id.clone());
            }
        }
        reopened.sort();
        reopened
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
    /// residual cycle survived (the DAG is normally acyclic after internal normalization);
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
        // Schedule ready peers in PLAN ORDER (top-to-bottom), so what RUNS matches what the
        // checklist SHOWS — a user reads the plan as an ordered list and expects step N to be
        // driven before step N+1. `ready_steps` already yields plan order, so we keep it.
        //
        // An earlier version reordered ready peers by DESCENDING blast radius (run the
        // higher-fan-out peer first). That surfaced as the repeatedly-reported "it skipped
        // task 3 and jumped to task 4": a frontend step (many dependents) outranked its
        // independent qa-test-authoring peer, which then sat `pending` while a later step ran —
        // and it ran code BEFORE its test-authoring peer, against test-first discipline.
        // Correctness is unaffected either way: `ready_steps` already guarantees every
        // returned step's dependencies are `Done`, so a step still never runs before a
        // prerequisite; this only chooses the order among independent, already-ready peers.
        self.ready_steps()
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
    /// `new_steps` is the brain's proposed replacement (parsed by the internal brain-step parser).
    ///
    /// The whole merged plan (the SURVIVING steps + the new steps) is re-validated
    /// through the EXISTING internal normalization machinery — dedup by id, dangling-dep
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
        // A re-plan merge re-normalises structurally but does NOT re-apply the
        // core-doc floor (`None`): a surviving PM/architect step already carries its
        // FileContains evidence from the initial synthesis (cloned intact), and the
        // merge has no route/slug context — so we never strip it, and a rare new PM
        // step in the sub-DAG simply reverts to today's behaviour (fail-open).
        let Some(mut normalized) = candidate.normalized(None) else {
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
    ///
    /// `doc_skeleton` carries `(slug, needs_ui)` ONLY on a DELIBERATE route
    /// (`Some((slug, needs_ui))`) — then TWO doc-first guarantees fire: first
    /// [`Self::ensure_doc_first_skeleton`] INSERTS the missing PM/architect (and, when
    /// `needs_ui`, UIUX) doc BUILD steps at the head of the plan and chains later code
    /// steps behind them, so a backend-first brain plan can never skip the three core
    /// docs; then the core-doc evidence floor binds each such seat to actually PRODUCE
    /// its PRD/architecture/UIUX doc (see [`Self::enforce_doc_evidence_floor`]). `None`
    /// (a lean/quick route, a re-plan merge, or a test) skips BOTH — the smaller path
    /// never demands the full doc set, and finalize honestly reports any missing doc
    /// rather than fabricating it.
    fn normalized(mut self, doc_skeleton: Option<(&str, bool)>) -> Option<Self> {
        let mut seen: HashSet<String> = HashSet::new();
        self.steps.retain(|s| {
            let id = s.id.trim();
            !id.is_empty() && seen.insert(id.to_string())
        });
        if self.steps.is_empty() {
            return None;
        }
        // DOC-FIRST SKELETON (deliberate builds only) — before any structural ordering,
        // seat the three core-doc BUILD steps (PRD / architecture / [UIUX]) at the head
        // of the plan when the brain's plan omitted them, so a weak base that jumped
        // straight to backend code is forced through docs first. Idempotent + fail-open;
        // `None` (lean / quick / re-plan / test) skips it. `enforce_contract_first`
        // (below) then wires the frontend/backend build steps behind the architect seat,
        // so code can't start before the docs.
        if let Some((slug, needs_ui)) = doc_skeleton {
            self.ensure_doc_first_skeleton(slug, needs_ui);
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
        // CORE-DOC EVIDENCE FLOOR (deliberate builds only) — when the plan convened a
        // PM/architect, bind that seat to actually PRODUCE its core doc via a
        // FileContains evidence contract, so the PRD/architecture is a VERIFIED build
        // deliverable (checked on the deterministic floor before the step ticks Done)
        // instead of a template stub retro-fitted at finalize. `None` (lean / quick /
        // re-plan / test) skips it. Augment-only + fail-open. See
        // [`Self::enforce_doc_evidence_floor`].
        //
        // Runs BEFORE the seat floor + the falsifiability backstop (F3): binding a
        // PM/architect/designer doc step to its FileContains/FileExists contract makes
        // it non-bare, so (1) the designer arm of the seat floor below never converts
        // a deliberate UIUX *doc-authoring* step into a design-tokens step (the doc
        // step is already contracted to produce the uiux doc; the SEPARATE tokens step
        // owns the tokens bar), and (2) falsifiability no longer wrongly pins
        // BuildClean onto a doc-authoring step (which can NEVER make the project build
        // - the plan would stall at its first doc step, reworking "make the build
        // clean" at a PM who writes no code).
        if let Some((slug, _needs_ui)) = doc_skeleton {
            self.enforce_doc_evidence_floor(slug);
        }
        // PER-SEAT DETERMINISTIC FLOOR (verification-level seat differentiation) —
        // orthogonal to the DAG above: augment an UNDER-SPECIFIED build step with a
        // seat-appropriate default (backend → FE↔BE contract, QA → tests pass,
        // frontend/security → build+lint clean, designer → the design-tokens
        // deliverable), then a falsifiability backstop when the brain under-specified
        // the plan wholesale. Augment-only + fail-open: a step already carrying a
        // strong contract, or a seat with no floor, is left exactly as today. See
        // [`Self::enforce_seat_evidence_floor`] / [`Self::enforce_falsifiability_floor`].
        self.enforce_seat_evidence_floor();
        self.enforce_falsifiability_floor();
        // RED→GREEN FLOOR (deliberate builds only) — the doc-first skeleton has just
        // guaranteed that QA AUTHORS the tests before any code step runs, so on this
        // path a code step's named test provably did not exist (let alone pass) before
        // the run. That makes the red→green bar checkable, so upgrade an
        // implementation step's `test-passes` claim into it. Opt-in by construction
        // (only a code step that NAMED a test is touched) and never global. See
        // [`Self::enforce_red_green_floor`].
        if doc_skeleton.is_some() {
            self.enforce_red_green_floor();
        }
        // EXECUTION-CONTRACT SURFACE. A path-bearing evidence contract already says
        // exactly which file the step must create/modify, so project that same path
        // into `files` when the brain omitted the redundant declaration. Generic
        // BuildClean/SourcePresent steps stay empty and are re-asked/blocked rather
        // than receiving a guessed broad directory.
        self.infer_execution_surfaces();
        self.risks.retain(|r| !r.trim().is_empty());
        self.open_questions.retain(|q| !q.trim().is_empty());
        Some(self)
    }

    /// Losslessly infer file surfaces from exact typed evidence. Kept as one method
    /// so fresh synthesis, re-plan normalisation, and old-plan loading share the same
    /// migration semantics.
    fn infer_execution_surfaces(&mut self) {
        for step in &mut self.steps {
            crate::execution_contract::infer_step_surface(step);
        }
    }

    /// **RED→GREEN FLOOR** — upgrade a code step's `test-passes <name>` claim into the
    /// falsifiable [`EvidenceContract::TestFailsThenPasses`].
    ///
    /// A completion claim is only worth the evidence behind it, and "a named test
    /// passes" is the weakest possible test evidence: a test authored AFTER the code,
    /// asserting whatever the code already does, passes on the first run and has never
    /// demonstrated that it can detect the behaviour's ABSENCE. The only mechanical
    /// difference between a real test and a rubber stamp is that a real test FAILED
    /// once — before the change that made it pass.
    ///
    /// This floor is deliberately NARROW. It only touches a step that is:
    /// 1. a BUILD step on a code seat (frontend / backend engineer) — the seats that
    ///    make a test go green; never QA (which AUTHORS the test, and whose authored
    ///    test is *expected* to be red at head), never a doc seat;
    /// 2. already claiming a SPECIFIC named test ([`EvidenceContract::TestPasses`] with
    ///    `Some(name)`) — a step that named no test is left exactly as it was; and
    /// 3. wired BEHIND the plan's QA test-authoring step (`depends_on` it), which the
    ///    doc-first skeleton guarantees on this path. That dependency is what makes the
    ///    pre-state meaningful: the test exists (QA wrote it) and the code does not, so
    ///    "red at the pre-state" is the truth we can hold the step to.
    ///
    /// The upgrade only ADDS a requirement (the pre-state must have been red); the
    /// head-state bar is the one the step already accepted. Verification itself is
    /// fail-open at every inconclusive edge (see [`EvidenceContract::TestFailsThenPasses`]),
    /// so a workspace where the temporary rewind or the test runner is unavailable
    /// degrades to exactly today's `test-passes` behaviour. Idempotent + pure.
    fn enforce_red_green_floor(&mut self) {
        use crate::critics::Seat;
        // The QA test-authoring anchor: a QA BUILD step that the code steps are wired
        // behind. Without one there is no guaranteed "the test existed and was red"
        // pre-state, so the floor does nothing (fail-open).
        let qa_ids: HashSet<String> = self
            .steps
            .iter()
            .filter(|s| s.kind == StepKind::Build && s.seat == Seat::QaEngineer)
            .map(|s| s.id.clone())
            .collect();
        if qa_ids.is_empty() {
            return;
        }
        for s in &mut self.steps {
            let is_code_build = s.kind == StepKind::Build
                && matches!(s.seat, Seat::FrontendEngineer | Seat::BackendEngineer);
            if !is_code_build || !s.depends_on.iter().any(|d| qa_ids.contains(d)) {
                continue;
            }
            // Rewrite each `test-passes <name>` in place, preserving order + dedupe.
            let mut upgraded: Vec<EvidenceContract> = Vec::with_capacity(s.evidence.len());
            for c in std::mem::take(&mut s.evidence) {
                let next = match c {
                    EvidenceContract::TestPasses { name: Some(n) } if !n.trim().is_empty() => {
                        EvidenceContract::TestFailsThenPasses {
                            test: n.trim().to_string(),
                        }
                    }
                    other => other,
                };
                push_unique(&mut upgraded, next);
            }
            s.evidence = upgraded;
        }
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
                let Some(frame) = stack.last_mut() else {
                    break;
                };
                frame.1 += 1;
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
    /// contract step (deduped, never a self-edge).
    ///
    /// **The contract author is DE-BIASED first**: before the consumer edges are
    /// added, every contract step's OWN `depends_on` entries pointing at consumer code
    /// steps are STRIPPED (mirroring [`Self::enforce_test_authoring_independence`]).
    /// A weak base sometimes plans "document the API after implementation" (the
    /// architect step depending on a backend step) — adding the consumer→contract
    /// edge over that would close a cycle, and [`Self::break_dependency_cycles`]
    /// drops an ARBITRARY DFS back-edge, which could be the contract-first edge
    /// itself: the code step then silently ran BEFORE the architecture doc (a
    /// docs-first bypass). Stripping the author's code deps means the cycle never
    /// forms, so the edge ORIENTATION (code depends on contract) deterministically
    /// survives. A be/fe-seated step that is ITSELF a contract step (acceptance =
    /// Contract) is not treated as a consumer code dep here. Any residual cycle from
    /// other shapes is still broken afterward by [`Self::break_dependency_cycles`],
    /// so the DAG always stays schedulable. No contract step ⇒ a deterministic no-op
    /// (fail-open). Pure over `self`.
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
        // De-bias the contract author: the interface is derived from the requirement,
        // never from the code that must build against it — strip a contract step's
        // deps on the consumer code steps so the code→contract orientation below can
        // never be inverted by an arbitrary cycle-break.
        let code_ids: HashSet<String> = self
            .steps
            .iter()
            .filter(|s| {
                s.kind == StepKind::Build
                    && matches!(s.seat, Seat::FrontendEngineer | Seat::BackendEngineer)
                    && !contract_ids.contains(&s.id)
            })
            .map(|s| s.id.clone())
            .collect();
        for s in &mut self.steps {
            if contract_ids.contains(&s.id) {
                s.depends_on.retain(|d| !code_ids.contains(d));
            }
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
    /// actually passing, a frontend/security step by a clean build+lint, a designer
    /// step by its real design-tokens deliverable) rather than only narratively. This
    /// AUGMENTS: it fires ONLY for a step carrying no contract stronger than
    /// source-present ([`PlanStep::has_strong_contract`]), so a brain-volunteered
    /// contract is never removed or downgraded. Every default is a check the existing
    /// deterministic floor already verifies — no new gate, no new [`EvidenceContract`]
    /// variant (the designer floor lives on the [`AcceptanceSpec`] axis, see below).
    /// A REVIEW step, a seat with no floor, or an already-contracted step is left
    /// exactly as today (fail-open). Bounded (one pass); pure over `self`.
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
                // Designer: the seat's anti-theatre floor is the DESIGN-TOKENS
                // deliverable (`design-tokens.{json,css}` as REAL files, verified by
                // `VerifyKind::DesignTokensPresent`). There is no EvidenceContract
                // variant for it — the checker lives on the AcceptanceSpec axis — so
                // the floor UPGRADES the step's weak acceptance in place. Still
                // augment-only: this arm is reached only for a bare step (acceptance
                // SourcePresent/TurnSettled, no strong evidence), and a deliberate
                // route's UIUX *doc* step was already bound to its uiux-doc file by
                // [`Self::enforce_doc_evidence_floor`] (which runs first), so a
                // doc-authoring step is never converted.
                Seat::UiuxDesigner => {
                    s.acceptance = AcceptanceSpec::DesignTokensPresent;
                    continue;
                }
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
        use crate::critics::Seat;
        // A DOC-authoring seat (PM / architect / designer) produces a doc, NOT a build, so it
        // is excluded from the falsifiability heuristic entirely (F3): otherwise a bare doc
        // step got BuildClean and could never pass - it writes no code that could make the
        // project build. Its OWN doc-evidence floor governs it.
        let is_code_build = |s: &PlanStep| {
            s.kind == StepKind::Build
                && !matches!(
                    s.seat,
                    Seat::ProductManager | Seat::Architect | Seat::UiuxDesigner
                )
        };
        let build_total = self.steps.iter().filter(|s| is_code_build(s)).count();
        let bare = self
            .steps
            .iter()
            .filter(|s| is_code_build(s) && !s.has_strong_contract())
            .count();
        // Trip only on STRICTLY more than half the code-build steps still bare.
        if build_total == 0 || bare * 2 <= build_total {
            return;
        }
        for s in &mut self.steps {
            if is_code_build(s) && !s.has_strong_contract() {
                push_unique(&mut s.evidence, EvidenceContract::BuildClean);
            }
        }
    }

    /// **Core-doc evidence floor (deliberate builds only)** — when the plan carries a
    /// PM or architect BUILD step, bind that seat to actually PRODUCE its core doc by
    /// attaching a [`EvidenceContract::FileContains`] on the doc file with the marker
    /// it must hold (`FR-` for the PRD's functional requirements; `API` for the
    /// architecture doc's API surface). This makes the PRD / architecture a VERIFIED
    /// deliverable of the build — the deterministic floor checks the file exists AND
    /// holds the marker before the step ticks [`StepStatus::Done`] — instead of a
    /// TODO-template stub retro-fitted at finalize (which masqueraded as real work and
    /// fed the `FR-`coverage check fabricated ids, making it vacuous). Augment-only +
    /// idempotent ([`push_unique`]): a stronger brain-volunteered contract is never
    /// removed or downgraded. If the plan has NO PM/architect BUILD step (a smaller
    /// deliberate build), it is a no-op — UmaDev does NOT invent a doc step, and
    /// finalize honestly reports the doc missing rather than fabricating it. Only fired
    /// on a deliberate route (the caller gates on `doc_slug`). Pure over `self`.
    fn enforce_doc_evidence_floor(&mut self, slug: &str) {
        use crate::critics::Seat;
        for s in &mut self.steps {
            // A REVIEW step reads the doc; only a BUILD step AUTHORS it.
            if s.kind != StepKind::Build {
                continue;
            }
            // The designer's design-tokens DELIVERABLE step is code-phase prep, not
            // the uiux DOC author — binding it to the uiux doc would make its
            // evidence non-empty, and the evidence path then bypasses its
            // DesignTokensPresent acceptance entirely (the tokens bar would silently
            // vanish). Its own acceptance governs it; skip.
            if is_design_tokens_step(s) {
                continue;
            }
            let contract = match s.seat {
                Seat::ProductManager => EvidenceContract::FileContains {
                    path: format!("output/{slug}-prd.md"),
                    needle: "FR-".to_string(),
                },
                Seat::Architect => EvidenceContract::FileContains {
                    path: format!("output/{slug}-architecture.md"),
                    needle: "API".to_string(),
                },
                Seat::UiuxDesigner => EvidenceContract::FileExists {
                    path: format!("output/{slug}-uiux.md"),
                },
                _ => continue, // every other seat authors no core narrative doc
            };
            push_unique(&mut s.evidence, contract);
        }
    }

    /// **Doc-first skeleton (deliberate builds only)** — GUARANTEE the plan opens with
    /// the three core-doc BUILD steps (PRD → architecture → [UIUX]) even when the
    /// brain's plan omitted them, so a weak base that decomposed the requirement into
    /// backend code with no docs is still forced through docs FIRST. For each core-doc
    /// seat missing from the plan (matched by seat + [`StepKind::Build`]; a designer
    /// design-tokens step never anchors the doc chain — see [`is_design_tokens_step`]),
    /// a step is synthesised and PREPENDED, chained in order (PRD → architecture →
    /// UIUX) so the docs run sequentially; a seat the brain ALREADY seated is left in
    /// place (its id still anchors the chain, so no duplicate is inserted). When
    /// `needs_ui`, the UIUX doc is part of the chain and every frontend BUILD step
    /// gains a `depends_on` on it (design system before the UI). Any code step is
    /// later wired behind the architect seat by [`Self::enforce_contract_first`], so
    /// implementation can never start before the docs land.
    ///
    /// **Two team deliverables are STRUCTURALLY guaranteed after the doc chain** (the
    /// planner prompt asks for them — belt — and this skeleton inserts them when the
    /// brain omitted them — suspenders). Both are CODE-PHASE PREP: they depend on
    /// EVERY doc anchor, so they only become ready after the whole doc family is Done
    /// — i.e. AFTER the `docs_confirm` gate fires and resumes, never inside the doc
    /// era (the gate's family/trigger detection also excludes them, see
    /// `confirm_gate_after_step`):
    ///
    /// 1. **QA test-authoring** (`umadev-phase-test-plan`, test-first): QA writes the
    ///    acceptance tests from the PRD/contract BEFORE any code exists; every
    ///    frontend/backend BUILD step gains a `depends_on` on it. Its evidence is
    ///    "the authored tests EXIST on disk" ([`EvidenceContract::FileExists`] on
    ///    [`TEST_AUTHORING_DIR`]) — deliberately NOT `TestPasses`: authored tests are
    ///    EXPECTED to fail until the code lands, so a green-suite bar here would
    ///    deadlock the plan. An existing brain QA BUILD step positioned before the
    ///    code steps is ADOPTED instead of duplicated when its own bar can run
    ///    pre-code ([`qa_step_precode_runnable`]); a bare adopted step gains the same
    ///    authored-tests evidence (pre-empting the seat floor's `TestPasses`, which
    ///    cannot pass pre-code). [`Self::enforce_test_authoring_independence`] (which
    ///    runs after) strips any QA→code edge, so the code→QA orientation
    ///    deterministically survives — never left to an arbitrary cycle-break.
    /// 2. **Designer design-tokens** (`umadev-phase-design-tokens`, UI-bearing builds
    ///    only): the designer ships the design system as REAL
    ///    `design-tokens.{json,css}` files (acceptance
    ///    [`AcceptanceSpec::DesignTokensPresent`], verified on the deterministic
    ///    floor) before the UI is built; every frontend BUILD step gains a
    ///    `depends_on` on it. An existing brain tokens step is adopted (deps wired,
    ///    not duplicated) and DE-BIASED: its own deps on code steps are stripped so
    ///    the frontend→tokens orientation survives.
    ///
    /// Idempotent (an already-complete plan only gains the ordering edges, deduped)
    /// and fail-open (no code step ⇒ the wiring is a no-op). Pure over `self`.
    fn ensure_doc_first_skeleton(&mut self, slug: &str, needs_ui: bool) {
        use crate::critics::Seat;
        let mut wanted: Vec<(Seat, &'static str, String, EvidenceContract)> = vec![
            (
                Seat::ProductManager,
                "umadev-phase-prd",
                "锁定产品需求文档 PRD（功能需求 FR-、验收口径、用户闭环路径）".to_string(),
                EvidenceContract::FileContains {
                    path: format!("output/{slug}-prd.md"),
                    needle: "FR-".to_string(),
                },
            ),
            (
                Seat::Architect,
                "umadev-phase-architecture",
                "输出架构文档（分层 / 分包、数据模型、API 契约、技术栈选型与理由）".to_string(),
                EvidenceContract::FileContains {
                    path: format!("output/{slug}-architecture.md"),
                    needle: "API".to_string(),
                },
            ),
        ];
        if needs_ui {
            wanted.push((
                Seat::UiuxDesigner,
                "umadev-phase-uiux",
                "输出 UI/UX 设计文档（设计令牌、排版系统、页面骨架、组件状态）".to_string(),
                EvidenceContract::FileExists {
                    path: format!("output/{slug}-uiux.md"),
                },
            ));
        }
        let mut inserted: Vec<PlanStep> = Vec::new();
        let mut prev_doc_id: Option<String> = None;
        // Every doc anchor (existing or inserted) — the "docs are locked" prerequisite
        // set the code-phase prep steps below are wired behind.
        let mut doc_anchor_ids: Vec<String> = Vec::new();
        let mut uiux_doc_id: Option<String> = None;
        for (seat, id, title, evidence) in wanted {
            // A designer step whose acceptance is DesignTokensPresent is the TOKENS
            // deliverable, not the UIUX doc author — it never anchors the doc chain
            // (otherwise a tokens-only plan would silently satisfy the uiux-doc seat).
            if let Some(existing) = self
                .steps
                .iter()
                .find(|s| s.seat == seat && s.kind == StepKind::Build && !is_design_prep_step(s))
            {
                prev_doc_id = Some(existing.id.clone());
                doc_anchor_ids.push(existing.id.clone());
                if seat == Seat::UiuxDesigner {
                    uiux_doc_id = Some(existing.id.clone());
                }
                continue;
            }
            let step = PlanStep {
                files: StepFiles::default(),
                id: id.to_string(),
                title,
                seat,
                kind: StepKind::Build,
                depends_on: prev_doc_id.iter().cloned().collect(),
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: vec![evidence],
                status: StepStatus::Pending,
            };
            prev_doc_id = Some(step.id.clone());
            doc_anchor_ids.push(step.id.clone());
            if seat == Seat::UiuxDesigner {
                uiux_doc_id = Some(step.id.clone());
            }
            inserted.push(step);
        }

        let is_code_step = |s: &PlanStep| {
            s.kind == StepKind::Build
                && matches!(s.seat, Seat::FrontendEngineer | Seat::BackendEngineer)
        };

        // ── 1. QA test-authoring step (test-first) ─────────────────────────────
        // Adopt the brain's own test-authoring step when it is a QA BUILD step
        // positioned BEFORE the code steps AND its bar can run pre-code; otherwise
        // insert the skeleton step. (Indexes are the brain's original order — the
        // inserted docs are only prepended at the end of this function.)
        let first_code_idx = self.steps.iter().position(&is_code_step);
        let qa_anchor: Option<String> = self
            .steps
            .iter()
            .enumerate()
            .find(|(i, s)| {
                s.kind == StepKind::Build
                    && s.seat == Seat::QaEngineer
                    && first_code_idx.is_none_or(|c| *i < c)
                    && qa_step_precode_runnable(s)
            })
            .map(|(_, s)| s.id.clone());
        let qa_id = if let Some(id) = qa_anchor {
            for s in &mut self.steps {
                if s.id == id {
                    // A bare adopted step gains the authored-tests evidence — this
                    // pre-empts the seat floor's TestPasses default, which could
                    // never pass before the code the tests check exists.
                    if !s.has_strong_contract() {
                        push_unique(
                            &mut s.evidence,
                            EvidenceContract::FileExists {
                                path: TEST_AUTHORING_DIR.to_string(),
                            },
                        );
                    }
                    // SCOPE: a QA step bound (above, or by the brain) to deliver its
                    // tests in `tests/` has said where it writes — record that as its
                    // declared surface so the scope check does not read its own
                    // structurally-required output as unplanned work. Only when the
                    // brain declared no surface of its own.
                    if s.files.is_empty() {
                        s.files.create.push(TEST_AUTHORING_DIR.to_string());
                    }
                    // Docs-first: test-authoring starts only after the docs are locked
                    // (and therefore after the docs_confirm gate, which fires when the
                    // doc family completes).
                    for d in &doc_anchor_ids {
                        if *d != s.id && !s.depends_on.contains(d) {
                            s.depends_on.push(d.clone());
                        }
                    }
                }
            }
            id
        } else {
            let step = PlanStep {
                // The step's evidence contract already binds it to deliver its tests in
                // `tests/`; declaring the same surface keeps the scope check from
                // reading UmaDev's own structurally-required step as unplanned work.
                files: StepFiles {
                    create: vec![TEST_AUTHORING_DIR.to_string()],
                    modify: Vec::new(),
                },
                id: "umadev-phase-test-plan".to_string(),
                title: "QA 先行编写验收测试（依据 PRD 功能需求与 API 契约盲写测试,先于前后端实现,\
                        测试文件写入 tests/ 目录;此时测试可以失败,代码落地后再转绿）"
                    .to_string(),
                seat: Seat::QaEngineer,
                kind: StepKind::Build,
                depends_on: doc_anchor_ids.clone(),
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: vec![EvidenceContract::FileExists {
                    path: TEST_AUTHORING_DIR.to_string(),
                }],
                status: StepStatus::Pending,
            };
            let id = step.id.clone();
            inserted.push(step);
            id
        };
        // TEST-FIRST ordering: every frontend/backend BUILD step builds AGAINST the
        // authored tests. The reverse (QA→code) edges are stripped by
        // `enforce_test_authoring_independence` (runs after this), so these edges can
        // never close a cycle — the orientation survives deterministically.
        for s in &mut self.steps {
            if is_code_step(s) && s.id != qa_id && !s.depends_on.contains(&qa_id) {
                s.depends_on.push(qa_id.clone());
            }
        }

        // ── 2. Designer VISUAL-DIRECTION step (UI-bearing builds only) ─────────
        // UD-CODE-007f. The designer used to jump straight to a tokens file whose
        // ONLY gate was existence — i.e. it answered "what hex?" before ever
        // answering "what is this, for whom, and what does it feel like?". A hex
        // chosen with no direction behind it is a guess, and a guess is exactly what
        // reads as AI-slop. So the plan now FORCES a decision step first, whose
        // evidence is a `## Visual direction` section in the UIUX doc carrying: a
        // one-line design read (page kind / audience / REGISTER / vibe / family),
        // three forced decisions (color commitment level; the theme decided by a
        // physical-scene sentence; 2-3 NAMED anchor references each bound to one
        // dimension — adjectives are rejected), and anti-goals.
        //
        // Excluded from the docs family (`is_design_prep_step`) like the tokens step:
        // it REFINES the UIUX doc rather than authoring it, so it never anchors the
        // doc chain and never re-fires `docs_confirm`.
        let direction_id: Option<String> = if needs_ui {
            let existing = self
                .steps
                .iter()
                .find(|s| is_design_direction_step(s))
                .map(|s| s.id.clone());
            let id = existing.unwrap_or_else(|| {
                let step = PlanStep {
                    files: StepFiles::default(),
                    id: DESIGN_DIRECTION_STEP_ID.to_string(),
                    title: "先定视觉方向,再谈颜色:在 UIUX 文档写出 `## Visual direction` \
                            段——(1) 一句话 design read(页面类型/受众/register 是 brand 还是 \
                            product/气质/美学家族);(2) 三个必须做出的决定:配色承诺度\
                            (restrained|committed|full-palette|drenched)、明暗由一句物理场景\
                            决定(谁在用、在哪、什么环境光、什么情绪——逼不出明暗就把场景写得更\
                            具体)、2-3 个具名锚点参照且每个绑定到一个具体维度(密度参照一个/\
                            排版参照另一个/留白参照第三个,\"现代\"\"干净\"这类形容词不算);\
                            (3) anti-goals(明确不做什么)"
                        .to_string(),
                    seat: Seat::UiuxDesigner,
                    kind: StepKind::Build,
                    depends_on: doc_anchor_ids.clone(),
                    acceptance: AcceptanceSpec::SourcePresent,
                    evidence: vec![EvidenceContract::FileContains {
                        path: format!("output/{slug}-uiux.md"),
                        needle: "## Visual direction".to_string(),
                    }],
                    status: StepStatus::Pending,
                };
                let id = step.id.clone();
                inserted.push(step);
                id
            });
            Some(id)
        } else {
            None
        };

        // ── 3. Designer design-tokens step (UI-bearing builds only) ────────────
        if needs_ui {
            let tokens_anchor: Option<String> = self
                .steps
                .iter()
                .find(|s| is_design_tokens_step(s))
                .map(|s| s.id.clone());
            let tokens_id = if let Some(id) = tokens_anchor {
                // De-bias the adopted tokens step (tokens precede the code that
                // imports them) + docs-first ordering — so the frontend→tokens
                // edges below can never close a cycle.
                let code_ids: Vec<String> = self
                    .steps
                    .iter()
                    .filter(|s| is_code_step(s))
                    .map(|s| s.id.clone())
                    .collect();
                for s in &mut self.steps {
                    if s.id == id {
                        s.depends_on.retain(|d| !code_ids.contains(d));
                        for d in &doc_anchor_ids {
                            if *d != s.id && !s.depends_on.contains(d) {
                                s.depends_on.push(d.clone());
                            }
                        }
                        // DIRECTION BEFORE TOKENS: a hex chosen with no direction
                        // behind it is a guess. The adopted tokens step is wired
                        // behind the direction step too.
                        if let Some(dir) = direction_id.as_ref() {
                            if *dir != s.id && !s.depends_on.contains(dir) {
                                s.depends_on.push(dir.clone());
                            }
                        }
                        // Raise an adopted tokens step from EXISTENCE to CONFORMANCE
                        // (UD-CODE-007): `:root{--color-bg:#000}` used to pass.
                        if s.acceptance == AcceptanceSpec::DesignTokensPresent {
                            s.acceptance = AcceptanceSpec::DesignTokensConform;
                        }
                    }
                }
                id
            } else {
                let mut depends_on = doc_anchor_ids.clone();
                if let Some(dir) = direction_id.as_ref() {
                    depends_on.push(dir.clone());
                }
                let step = PlanStep {
                    files: StepFiles::default(),
                    id: "umadev-phase-design-tokens".to_string(),
                    title: "输出设计令牌文件 design-tokens.{json,css}（按 Visual direction 落地:\
                            OKLCH 色板且每个 surface 配一个 `on-` 前景并算过 WCAG 对比度、\
                            ≥4 级字阶(相邻比例 1.125-1.2)、4pt 间距刻度、圆角刻度、\
                            ≥2 个动效时长 + ≥1 条缓动曲线,先于前端实现锁定设计系统,\
                            供前端直接 import）"
                        .to_string(),
                    seat: Seat::UiuxDesigner,
                    kind: StepKind::Build,
                    depends_on,
                    // The tokens bar lives on the AcceptanceSpec axis
                    // (VerifyKind::DesignSystemConform); evidence stays empty so the
                    // verifier takes the acceptance path, not the evidence path.
                    acceptance: AcceptanceSpec::DesignTokensConform,
                    evidence: Vec::new(),
                    status: StepStatus::Pending,
                };
                let id = step.id.clone();
                inserted.push(step);
                id
            };
            // Design system before the UI: every frontend BUILD step imports the
            // tokens, so it depends on them.
            for s in &mut self.steps {
                if s.kind == StepKind::Build
                    && s.seat == Seat::FrontendEngineer
                    && s.id != tokens_id
                    && !s.depends_on.contains(&tokens_id)
                {
                    s.depends_on.push(tokens_id.clone());
                }
            }
        }

        // Design DOC before the UI: every frontend BUILD step also depends on the
        // UIUX doc anchor. The adopted anchor is de-biased first (its own deps on
        // code steps stripped — a doc derives from the requirement, never from the
        // code built against it), so this edge can never be inverted by an arbitrary
        // cycle-break.
        if let Some(ref uiux) = uiux_doc_id {
            let code_ids: Vec<String> = self
                .steps
                .iter()
                .filter(|s| is_code_step(s))
                .map(|s| s.id.clone())
                .collect();
            for s in &mut self.steps {
                if &s.id == uiux {
                    s.depends_on.retain(|d| !code_ids.contains(d));
                } else if s.kind == StepKind::Build
                    && s.seat == Seat::FrontendEngineer
                    && !s.depends_on.contains(uiux)
                {
                    s.depends_on.push(uiux.clone());
                }
            }
        }
        if inserted.is_empty() {
            return;
        }
        let mut head = inserted;
        head.append(&mut self.steps);
        self.steps = head;
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

const MAX_PERSISTED_PLAN_BYTES: u64 = 8 * 1024 * 1024;

/// Persist a plan to `.umadev/plan.json` (atomic: write a temp sibling, then
/// rename). Best-effort + fail-open: any IO error is returned for the caller to
/// ignore — a failed persist never blocks the build. Returns `Ok(path)` on success.
pub fn save(plan: &Plan, root: &Path) -> std::io::Result<PathBuf> {
    let dir = crate::bounded_fs::ensure_real_dir_beneath(root, Path::new(".umadev"))?;
    let final_path = dir.join("plan.json");
    let json = serde_json::to_string_pretty(plan)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if json.len() > usize::try_from(MAX_PERSISTED_PLAN_BYTES).unwrap_or(usize::MAX) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "persisted plan exceeds the managed file limit",
        ));
    }
    umadev_state::fs::atomic_write(&final_path, json.as_bytes())?;
    Ok(root.join(plan_rel_path()))
}

/// Load the persisted plan from `.umadev/plan.json`, or `None` when absent /
/// unreadable / unparseable (fail-open — a corrupt plan is treated as "no plan").
#[must_use]
pub fn load(root: &Path) -> Option<Plan> {
    let text = String::from_utf8(
        umadev_state::fs::read_bounded_beneath(root, &plan_rel_path(), MAX_PERSISTED_PLAN_BYTES)
            .ok()?,
    )
    .ok()?;
    let mut plan = serde_json::from_str::<Plan>(&text).ok()?;
    // Backward-compatible migration for plans written before file surfaces became
    // a hard execution contract. Exact FileExists/FileContains evidence can be
    // projected without guessing; any genuinely under-specified Build step remains
    // empty and resume preflight reports it before opening a writer.
    plan.infer_execution_surfaces();
    Some(plan)
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
    /// The brain's REQUIRED file surface for this step
    /// (`{"create":[…],"modify":[…]}`) — the declaration the execution contract
    /// holds the working tree against. Parsed as a raw value so one sloppy field does
    /// not lose the rest of the plan; exact evidence may repair it, otherwise
    /// execution preflight blocks and asks for a corrected plan.
    #[serde(default)]
    files: serde_json::Value,
}

/// Normalise the brain's proposed per-step file surface into an owned [`StepFiles`].
///
/// Tolerant at the parsing boundary — a malformed declaration becomes empty so the
/// whole JSON reply remains inspectable, then the explicit execution-contract
/// preflight repairs/rejects it before mutation:
/// - `{"create":["a"],"modify":["b"]}` — the canonical object form.
/// - `["a","b"]` — a bare array is read as `modify` (the conservative reading: an
///   un-annotated path is not claimed as a NEW file).
/// - anything else (a string, a number, `null`, a missing key) → empty.
///
/// Each entry is trimmed, `\`-separated paths are normalised to `/`, a leading `./`
/// is stripped, empties are dropped, and the list is capped at
/// [`MAX_DECLARED_PATHS`] per bucket so a runaway reply cannot blow the check up.
fn parse_brain_files(v: &serde_json::Value) -> StepFiles {
    let list = |val: Option<&serde_json::Value>| -> Vec<String> {
        val.and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter_map(normalize_declared_path)
                    .take(MAX_DECLARED_PATHS)
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(obj) = v.as_object() {
        return StepFiles {
            create: list(obj.get("create").or_else(|| obj.get("new"))),
            modify: list(obj.get("modify").or_else(|| obj.get("edit"))),
        };
    }
    if v.is_array() {
        // A bare array names paths without saying whether they are new. Read them as
        // `modify` — the conservative bucket (claiming them as `create` would let an
        // unplanned NEW file hide behind a vague declaration).
        return StepFiles {
            create: Vec::new(),
            modify: list(Some(v)),
        };
    }
    StepFiles::default()
}

/// Per-bucket cap on a step's declared paths — a bound on the brain's declaration so
/// the scope check's matching work stays proportional to the plan, not to a runaway
/// reply.
const MAX_DECLARED_PATHS: usize = 64;

/// Canonicalise ONE declared path: trim, normalise `\` → `/`, strip a leading `./`
/// and any leading `/` (declarations are repo-relative), and reject an empty result
/// or an absolute / parent-escaping path (a declaration can only claim things INSIDE
/// the workspace). `None` ⇒ the entry is dropped.
fn normalize_declared_path(raw: &str) -> Option<String> {
    let p = raw.trim().replace('\\', "/");
    let p = p.trim_start_matches("./").trim_start_matches('/').trim();
    if p.is_empty() || p.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(p.to_string())
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
    drain_plan_turn_traced(session, directive, deadline, None)
        .await
        .text
}

struct TracedPlanTurn {
    text: Option<String>,
    recipe_receipt: Option<(PathBuf, crate::recipes::RecipeReceipt)>,
}

/// The plan drain with an optional prepared recipe prior. The receipt is committed
/// only *after* `send_turn` accepts the complete directive, which is the exact
/// delivery seam; a lookup or a failed transport write is never counted as use.
async fn drain_plan_turn_traced(
    session: &mut dyn BaseSession,
    directive: String,
    deadline: std::time::Instant,
    prepared_recipe: Option<(PathBuf, crate::recipes::PreparedRecipePrior)>,
) -> TracedPlanTurn {
    use umadev_runtime::{ApprovalDecision, SessionEvent};
    if session.send_turn(directive.clone()).await.is_err() {
        return TracedPlanTurn {
            text: None,
            recipe_receipt: None,
        };
    }
    let recipe_receipt = prepared_recipe.and_then(|(dir, prepared)| {
        crate::recipes::commit_recipe_prior_sent(&dir, prepared, &directive)
            .map(|receipt| (dir, receipt))
    });
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
                    return TracedPlanTurn {
                        text: None,
                        recipe_receipt,
                    };
                }
            }
            // claude/codex route AskUserQuestion / ExitPlanMode / elicitation as a
            // HostRequest, NEVER a NeedApproval — so without this arm the request
            // fell into the `Ok(Some(_))` ignore below, the base's callback never
            // got a control_response, the base blocked until the 180s idle timer,
            // and the `Err(_)` arm returned WITHOUT interrupting, handing the
            // fallback build a shared session that still carried an unanswered RPC
            // (exactly the wedge the NeedApproval arm above exists to prevent).
            // A JSON-only plan turn forbids tools: safe-reject it and, if the
            // reply cannot be written, interrupt to un-wedge the session.
            Ok(Some(SessionEvent::HostRequest { req_id, request })) => {
                let rejection = request.safe_rejection("plan-only turn does not run tools");
                if session.respond_host(&req_id, rejection).await.is_err() {
                    let _ = session.interrupt().await;
                    return TracedPlanTurn {
                        text: None,
                        recipe_receipt,
                    };
                }
            }
            // A JSON-only plan turn should emit no other tools; ignore anything else
            // and let the next-event timeout bound a misbehaving turn.
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                return TracedPlanTurn {
                    text: None,
                    recipe_receipt,
                }
            }
        }
    }
    let text = text.trim().to_string();
    TracedPlanTurn {
        text: (!text.is_empty()).then_some(text),
        recipe_receipt,
    }
}

/// Plan plus the exact recipe receipt (when a prior was actually accepted by the
/// base transport). The receipt survives parse failure so the final fallback build
/// can still settle that sent influence honestly.
pub(crate) struct PlanSynthesis {
    pub plan: Option<Plan>,
    pub recipe_receipt: Option<(PathBuf, crate::recipes::RecipeReceipt)>,
}

async fn repair_execution_surfaces(
    session: &mut dyn BaseSession,
    plan: &mut Plan,
    missing: &[String],
    deadline: std::time::Instant,
) {
    let current = serde_json::to_string_pretty(plan).unwrap_or_default();
    let repair = format!(
        "The plan below is structurally valid, but its execution contract is incomplete. \
         Build step id(s) [{}] have no `files` surface. Return the SAME full JSON plan \
         with the SAME step ids, titles, seats, dependencies, acceptance and evidence; \
         only fill each missing `files` object as \
         {{\"create\":[\"workspace/relative/path\"],\"modify\":[\"workspace/relative/path\"]}}. \
         Every path must be proportional to the requirement; never use `.`, `/`, `**`, \
         or a repository-wide catch-all. Return exactly one JSON object, no markdown, \
         prose, tools, commands, or file writes.\n\nCurrent plan:\n{current}",
        missing.join(", ")
    );
    let Some(repair_text) = drain_plan_turn(session, repair, deadline).await else {
        return;
    };
    let Some(repair_json) = crate::continuous::extract_json_object(&repair_text) else {
        return;
    };
    let Ok(repaired) = serde_json::from_str::<BrainPlan>(&repair_json) else {
        return;
    };
    let surfaces: std::collections::HashMap<String, StepFiles> = repaired
        .steps
        .into_iter()
        .filter_map(|step| {
            let id = step.id.trim().to_string();
            let files = parse_brain_files(&step.files);
            (!id.is_empty() && !files.is_empty()).then_some((id, files))
        })
        .collect();
    for step in &mut plan.steps {
        if step.kind != StepKind::Build || !step.files.is_empty() {
            continue;
        }
        let Some(files) = surfaces.get(&step.id) else {
            continue;
        };
        step.files = files.clone();
    }
    plan.infer_execution_surfaces();
}

/// Traced planning entry used by the director. Unlike the compatibility wrapper,
/// it returns the exact sent-prior receipt for terminal PASS/FAIL settlement.
pub(crate) async fn synthesize_plan_traced(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    requirement: &str,
    route: &RoutePlan,
    // LOW #5: the SHARED run deadline. The planning drain is bounded by it so the
    // whole deliberate build (planning + build) shares one clock instead of the old
    // fixed 180s-per-event timeout that left planning unattributed to the budget.
    deadline: std::time::Instant,
) -> PlanSynthesis {
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
         {{\"kind\":\"contract-matches\"}}, {{\"kind\":\"source-present\"}}, \
         {{\"kind\":\"test-fails-then-passes\",\"test\":\"login_rejects_bad_password\"}}. \
         Prefer the most \
         specific evidence the step actually produces (a concrete file/route over a generic \
         build-clean). \
         `test-fails-then-passes` is the strongest bar available and the right one for an \
         IMPLEMENTATION step that makes a previously-authored test go green: UmaDev rewinds \
         to the step's pre-state, runs THAT ONE named test (it must FAIL there), restores \
         head, and runs it again (it must PASS). Name a test that genuinely could not have \
         passed before this step. Never put it on the step that AUTHORS the test — an \
         authored test is expected to be red until the code lands. \
         `files` (REQUIRED for EVERY build step): the step's complete file surface, \
         {{\"create\":[\"src/api/login.ts\"],\"modify\":[\"src/routes.ts\"]}} — the paths this \
         step will bring into existence or edit (a trailing `/` claims a whole directory). \
         UmaDev holds the working tree against the UNION of every step's surface: a NEW \
         source file, dependency, public route, OR edit that NO step claimed is \
         out-of-scope work and blocks. A build step with no file surface cannot start. \
         Declare every file/directory the step may write; if a change is genuinely needed, \
         it belongs in some step's surface. Never use a repository-wide catch-all such as \
         `.` or `/`; keep the surface proportional to this requirement. \
         Team-deliverable rules (UmaDev enforces ALL of these STRUCTURALLY after parsing — \
         a missing tokens/QA step is inserted and the ordering edges are wired — so a plan \
         that already honours them survives normalisation unchanged): \
         (a) when there is a UI surface, the uiux-designer has a BUILD step that writes the \
         design system as real `design-tokens.{{json,css}}` files (acceptance=design-tokens) \
         and every frontend step depends_on it; \
         (b) the architect's API contract is a depends_on PREREQUISITE of every \
         frontend/backend step (lock the interface first); \
         (c) QA AUTHORS tests as its OWN build step BEFORE the code steps (seat=qa-engineer, \
         kind=build, acceptance=source-present, evidence=file-exists on the test dir/files — \
         authored tests are EXPECTED to fail until the code lands, so never gate this step \
         on a green suite) that does NOT depend on the frontend/backend code steps, while \
         every frontend/backend step depends_on it — the test-author must not be the \
         code-author (de-biasing); a QA review step is separate. \
         JSON shape: {{\"steps\":[{{\"id\":\"scaffold\",\"title\":\"…\",\"seat\":\"…\",\
         \"kind\":\"build\",\"depends_on\":[],\"acceptance\":\"source-present\",\
         \"evidence\":[{{\"kind\":\"file-exists\",\"path\":\"src/App.tsx\"}}],\
         \"files\":{{\"create\":[\"src/App.tsx\"],\"modify\":[]}}}}],\
         \"risks\":[\"…\"],\"open_questions\":[\"…\"]}}",
        class = route.class.as_str(),
        kind = route.kind.id(),
        depth = route.depth.as_str(),
    );
    let user = format!("Requirement:\n{requirement}");

    // SUCCESS-RECIPE RECALL (a PRIOR, never a template): look up the closest past
    // CLEAN build of a similar stack/kind/feature in this project's private recipe store
    // and inject its proven plan SHAPE as an adaptable hint the brain may reuse or
    // ignore. Fail-open at every step — no safe store, no match, or a read error yields
    // an empty prior, and the plan is synthesised exactly as before. Bounded to one
    // best recipe within [`crate::recipes::RECIPE_PRIOR_BUDGET`].
    let recipe_dir = if crate::memory_control::recall_enabled(
        &options.project_root,
        crate::memory_control::MemoryScope::Project,
        crate::memory_control::MemoryStore::Recipes,
    ) {
        crate::recipes::project_recipes_dir(&options.project_root)
    } else {
        None
    };
    let prepared_recipe = recipe_dir.as_ref().and_then(|dir| {
        let fp = crate::recipes::fingerprint_for(&options.project_root, route, requirement);
        crate::recipes::prepare_recipe_prior(dir, &fp, crate::recipes::RECIPE_PRIOR_BUDGET)
    });
    let recipe_prior = prepared_recipe
        .as_ref()
        .map(|prior| format!("{}\n\n", prior.block()))
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
    let traced = drain_plan_turn_traced(
        session,
        directive,
        deadline,
        recipe_dir.zip(prepared_recipe),
    )
    .await;
    let recipe_receipt = traced.recipe_receipt;
    let Some(text) = traced.text else {
        return PlanSynthesis {
            plan: None,
            recipe_receipt,
        };
    };
    let Some(json) = crate::continuous::extract_json_object(&text) else {
        return PlanSynthesis {
            plan: None,
            recipe_receipt,
        };
    };
    let Ok(raw) = serde_json::from_str::<BrainPlan>(&json) else {
        return PlanSynthesis {
            plan: None,
            recipe_receipt,
        };
    };
    let plan = Plan {
        steps: raw.steps.into_iter().map(brain_step_to_plan_step).collect(),
        risks: raw.risks,
        open_questions: raw.open_questions,
    };
    // Enforce the DOC-FIRST skeleton + core-doc evidence floor ONLY on a deliberate
    // route; a lean/quick build never demands the full doc set. The skeleton SEATS the
    // three core-doc steps up front (so a backend-first brain plan can't skip them) and
    // the floor binds each such seat to actually produce its PRD/architecture/UIUX (a
    // verified deliverable), so the docs are real up front — never a template stub
    // retro-fitted at finalize. `needs_ui` is true only for a UI-bearing build.
    let doc_skeleton = route
        .depth
        .is_deliberate()
        .then(|| (options.effective_slug(), route.needs_ui()));
    let Some(mut plan) = plan.normalized(doc_skeleton.as_ref().map(|(s, ui)| (s.as_str(), *ui)))
    else {
        return PlanSynthesis {
            plan: None,
            recipe_receipt,
        };
    };

    // EXECUTION-CONTRACT REPAIR (one bounded JSON-only re-ask). Exact evidence
    // inference above repairs many redundant omissions. For any Build step that is
    // still missing a surface, do not silently disable the scope floor and do not
    // throw away/rewrite the DAG: ask the same coordinator ONCE to fill only the
    // missing `files` objects, then merge those fields back by stable step id. If the
    // repair is unavailable or still vague, return the inspectable plan unchanged;
    // the writer preflight will fail explicitly before mutation.
    let missing: Vec<String> = plan
        .steps
        .iter()
        .filter(|step| step.kind == StepKind::Build && step.files.is_empty())
        .map(|step| step.id.clone())
        .collect();
    let repair_has_time = deadline.saturating_duration_since(std::time::Instant::now())
        >= std::time::Duration::from_secs(5);
    if !missing.is_empty() && repair_has_time {
        repair_execution_surfaces(session, &mut plan, &missing, deadline).await;
    }
    if let Some((dir, receipt)) = &recipe_receipt {
        let _ = crate::recipes::bind_recipe_receipt_to_plan(dir, receipt, &plan);
    }
    PlanSynthesis {
        plan: Some(plan),
        recipe_receipt,
    }
}

/// Backward-compatible planning API. Callers that do not participate in the
/// delivery lifecycle cannot attribute an eventual result, so any sent recipe prior
/// is immediately settled UNKNOWN instead of left pending or guessed successful.
pub async fn synthesize_plan(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    requirement: &str,
    route: &RoutePlan,
    deadline: std::time::Instant,
) -> Option<Plan> {
    let synthesis = synthesize_plan_traced(session, options, requirement, route, deadline).await;
    if let Some((dir, receipt)) = &synthesis.recipe_receipt {
        let _ = crate::recipes::settle_recipe_receipt(
            dir,
            receipt,
            crate::recipes::RecipeOutcome::Unknown,
        );
    }
    synthesis.plan
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
        files: parse_brain_files(&b.files),
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
            files: StepFiles::default(),
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
            files: StepFiles::default(),
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

    fn status_of(p: &Plan, id: &str) -> StepStatus {
        p.steps.iter().find(|s| s.id == id).unwrap().status
    }

    #[test]
    fn invalidate_stale_reopens_consumers_and_their_downstream() {
        use crate::critics::ArtifactKind as A;
        let mut p = plan(vec![
            step_seat(
                "arch",
                &[],
                Seat::Architect,
                StepKind::Build,
                AcceptanceSpec::TurnSettled,
            ),
            step_seat(
                "be",
                &["arch"],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::TurnSettled,
            ),
            step_seat(
                "qa",
                &["be"],
                Seat::QaEngineer,
                StepKind::Review,
                AcceptanceSpec::ReviewClean,
            ),
        ]);
        for s in &mut p.steps {
            s.status = StepStatus::Done;
        }
        let reopened = p.invalidate_stale(&[A::Architecture]);
        assert_eq!(reopened, vec!["be".to_string(), "qa".to_string()]);
        assert_eq!(status_of(&p, "arch"), StepStatus::Done);
        assert_eq!(status_of(&p, "be"), StepStatus::Pending);
        assert_eq!(status_of(&p, "qa"), StepStatus::Pending);
        for s in &mut p.steps {
            s.status = StepStatus::Done;
        }
        assert!(p.invalidate_stale(&[]).is_empty());
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
        .normalized(None)
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
        let p = plan(vec![step("", &[])]).normalized(None);
        assert!(p.is_none());
    }

    #[test]
    fn normalize_keeps_a_dep_edge_when_the_prereq_id_has_whitespace() {
        // M5 regression: a brain step id with surrounding whitespace (" auth ") must
        // still satisfy a dependent's (trimmed) ref to it. The id-set is built from the
        // TRIMMED ids, so the edge is NOT dropped as dangling — otherwise the dependent
        // would run BEFORE its prerequisite.
        let p = plan(vec![step(" auth ", &[]), step("ui", &["auth"])])
            .normalized(None)
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
            .normalized(None)
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
        .normalized(None)
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

    #[test]
    fn load_rejects_an_oversized_plan() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plan.json"),
            vec![b' '; usize::try_from(MAX_PERSISTED_PLAN_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert!(load(tmp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_and_load_refuse_symlinked_plan_state() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        let victim = outside.path().join("victim.json");
        std::fs::write(&victim, "outside must survive").unwrap();
        symlink(&victim, tmp.path().join(".umadev/plan.json")).unwrap();

        let p = plan(vec![step("a", &[])]);
        assert!(save(&p, tmp.path()).is_err());
        assert!(load(tmp.path()).is_none());
        assert_eq!(
            std::fs::read_to_string(victim).unwrap(),
            "outside must survive"
        );

        let linked_root = tempfile::TempDir::new().unwrap();
        let linked_outside = tempfile::TempDir::new().unwrap();
        let outside_plan = linked_outside.path().join("plan.json");
        std::fs::write(&outside_plan, "outside ancestor must survive").unwrap();
        symlink(linked_outside.path(), linked_root.path().join(".umadev")).unwrap();
        assert!(save(&p, linked_root.path()).is_err());
        assert!(load(linked_root.path()).is_none());
        assert_eq!(
            std::fs::read_to_string(outside_plan).unwrap(),
            "outside ancestor must survive"
        );
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
        send_fails: bool,
    }

    impl ScriptedSession {
        fn new(events: Vec<umadev_runtime::SessionEvent>, respond_fails: bool) -> Self {
            Self {
                events: events.into_iter().collect(),
                responded: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                interrupts: std::sync::Arc::new(std::sync::Mutex::new(0)),
                respond_fails,
                send_fails: false,
            }
        }

        fn failing_send() -> Self {
            let mut session = Self::new(Vec::new(), false);
            session.send_fails = true;
            session
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for ScriptedSession {
        async fn send_turn(
            &mut self,
            _directive: String,
        ) -> Result<(), umadev_runtime::SessionError> {
            if self.send_fails {
                Err(umadev_runtime::SessionError::Send(
                    "scripted send failure".into(),
                ))
            } else {
                Ok(())
            }
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
    async fn repair_execution_surfaces_merges_only_missing_build_files() {
        use umadev_runtime::{SessionEvent, TurnStatus};
        let mut plan = plan(vec![step("api", &[]), step("ui", &[])]);
        plan.steps[1].files.modify.push("src/ui.rs".into());
        let mut session = ScriptedSession::new(
            vec![
                SessionEvent::TextDelta(
                    r#"{"steps":[{"id":"api","files":{"create":["src/api.rs"]}},{"id":"ui","files":{"modify":["unexpected.rs"]}}]}"#
                        .into(),
                ),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ],
            false,
        );
        repair_execution_surfaces(
            &mut session,
            &mut plan,
            &["api".into()],
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await;
        assert_eq!(plan.steps[0].files.create, ["src/api.rs"]);
        assert_eq!(plan.steps[1].files.modify, ["src/ui.rs"]);
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
    async fn drain_plan_turn_safe_rejects_a_host_request_and_finishes_without_wedging() {
        use umadev_runtime::{ApprovalDecision, HostRequest, SessionEvent, TurnStatus};
        // claude/codex route AskUserQuestion / ExitPlanMode as a HostRequest,
        // never a NeedApproval. Without the HostRequest arm the base blocked to
        // the idle timeout and the shared session was handed on carrying an
        // unanswered RPC. drain must safe-reject it (recorded as a Deny through
        // the default respond_host) and still drain to the JSON reply.
        let mut s = ScriptedSession::new(
            vec![
                SessionEvent::HostRequest {
                    req_id: "hr-1".into(),
                    request: HostRequest::UserInput {
                        questions: Vec::new(),
                        metadata: serde_json::Value::Null,
                    },
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
            "the host request was answered, not left dangling"
        );
        assert_eq!(replies[0], ("hr-1".to_string(), ApprovalDecision::Deny));
    }

    #[tokio::test]
    async fn recipe_use_is_counted_only_after_plan_transport_accepts_directive() {
        use umadev_runtime::{SessionEvent, TurnStatus};
        let tmp = tempfile::TempDir::new().unwrap();
        let recipe = crate::recipes::Recipe {
            fingerprint: crate::recipes::Fingerprint {
                stack: "node".into(),
                kind: "greenfield".into(),
                shape: vec!["todo".into()],
            },
            plan_skeleton: vec!["frontend-engineer · scaffold".into()],
            key_scaffold: Vec::new(),
            patterns: Vec::new(),
            stats: crate::recipes::OutcomeStats::default(),
        };
        assert!(crate::recipes::capture_recipe(tmp.path(), recipe));
        let query = crate::recipes::Fingerprint {
            stack: "node".into(),
            kind: "greenfield".into(),
            shape: vec!["todo".into()],
        };

        let prepared = crate::recipes::prepare_recipe_prior(
            tmp.path(),
            &query,
            crate::recipes::RECIPE_PRIOR_BUDGET,
        )
        .unwrap();
        let directive = format!("{}\nfinal plan prompt", prepared.block());
        let mut rejected = ScriptedSession::failing_send();
        let rejected = drain_plan_turn_traced(
            &mut rejected,
            directive,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            Some((tmp.path().to_path_buf(), prepared)),
        )
        .await;
        assert!(rejected.recipe_receipt.is_none());
        assert_eq!(
            crate::recipes::load_recipes(tmp.path())[0]
                .stats
                .times_reused,
            0,
            "a failed send is not a use"
        );

        let prepared = crate::recipes::prepare_recipe_prior(
            tmp.path(),
            &query,
            crate::recipes::RECIPE_PRIOR_BUDGET,
        )
        .unwrap();
        let directive = format!("{}\nfinal plan prompt", prepared.block());
        let mut accepted = ScriptedSession::new(
            vec![
                SessionEvent::TextDelta("{\"steps\":[]}".into()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ],
            false,
        );
        let accepted = drain_plan_turn_traced(
            &mut accepted,
            directive,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
            Some((tmp.path().to_path_buf(), prepared)),
        )
        .await;
        assert!(accepted.recipe_receipt.is_some());
        let stats = &crate::recipes::load_recipes(tmp.path())[0].stats;
        assert_eq!(stats.times_reused, 1);
        assert_eq!(stats.pending_reuses, 1);
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
    fn ready_steps_prioritized_keeps_plan_order_among_ready_peers() {
        // Independent ready peers are scheduled in PLAN ORDER (top-to-bottom), so what RUNS
        // matches what the checklist SHOWS — no "it skipped task 3 and jumped to task 4". An
        // earlier blast-radius reorder ran a high-fan-out peer before an earlier one.
        let mut p = plan(vec![
            step("config", &[]),      // first in plan order
            step("schema", &[]),      // second; api + ui depend on it
            step("api", &["schema"]), // not ready until schema is Done
            step("ui", &["schema"]),
        ]);
        // Plain readiness is plan order: config, schema (api/ui gated by schema).
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["config", "schema"]);
        // Prioritised readiness KEEPS plan order: config BEFORE schema.
        let prio: Vec<_> = p
            .ready_steps_prioritized()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        assert_eq!(prio, vec!["config", "schema"]);
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
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
    fn load_migrates_exact_evidence_into_the_execution_surface() {
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
        assert_eq!(loaded.steps[0].evidence, p.steps[0].evidence);
        assert_eq!(
            loaded.steps[0].files.create,
            ["src/App.tsx"],
            "legacy plans gain the same exact path their verifier already requires"
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
        .expect("usable");
        assert!(
            ev_of(&p, "harden").contains(&EvidenceContract::BuildClean),
            "a bare security doing step references a real build bar: {:?}",
            ev_of(&p, "harden")
        );
    }

    #[test]
    fn deliberate_pm_and_architect_steps_gain_core_doc_evidence() {
        // HONESTY: on a DELIBERATE route (`normalized(Some(slug))`), a PM build step is
        // bound to actually PRODUCE the PRD (FileContains prd, "FR-") and an architect
        // build step to produce the architecture doc — a VERIFIED deliverable, not a
        // template stub retro-fitted at finalize. A frontend peer keeps the plan below
        // the wholesale falsifiability backstop so the assertion is about the doc floor.
        let p = plan(vec![
            step_seat(
                "prd",
                &[],
                Seat::ProductManager,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "arch",
                &[],
                Seat::Architect,
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
        .normalized(Some(("demo", true)))
        .expect("usable");
        assert!(
            ev_of(&p, "prd").contains(&EvidenceContract::FileContains {
                path: "output/demo-prd.md".to_string(),
                needle: "FR-".to_string(),
            }),
            "a deliberate PM build step must produce the PRD with FR- content: {:?}",
            ev_of(&p, "prd")
        );
        assert!(
            ev_of(&p, "arch").contains(&EvidenceContract::FileContains {
                path: "output/demo-architecture.md".to_string(),
                needle: "API".to_string(),
            }),
            "a deliberate architect build step must produce the architecture doc: {:?}",
            ev_of(&p, "arch")
        );
    }

    #[test]
    fn doc_first_skeleton_inserts_the_three_core_docs_before_a_backend_only_plan() {
        // DOC-FIRST ENFORCEMENT: a brain plan that jumped straight to a single backend
        // BUILD step (no docs) must, on a deliberate UI-bearing route, gain the three
        // core-doc BUILD steps (PRD → architecture → UIUX) PREPENDED and chained, with
        // the right evidence paths — and the backend code step must depend on the
        // architect doc (so code can't start before the docs).
        let p = plan(vec![step_seat(
            "api",
            &[],
            Seat::BackendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized(Some(("demo", true)))
        .expect("usable");
        // All three doc steps were seated.
        for id in [
            "umadev-phase-prd",
            "umadev-phase-architecture",
            "umadev-phase-uiux",
        ] {
            assert!(
                p.steps.iter().any(|s| s.id == id),
                "the {id} doc step was inserted: {:?}",
                p.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
            );
        }
        // The plan OPENS with the PRD step (docs first).
        assert_eq!(
            p.steps[0].id, "umadev-phase-prd",
            "the plan opens with the PRD"
        );
        // Each doc step carries its verified-deliverable evidence path.
        assert!(
            ev_of(&p, "umadev-phase-prd").contains(&EvidenceContract::FileContains {
                path: "output/demo-prd.md".to_string(),
                needle: "FR-".to_string(),
            })
        );
        assert!(
            ev_of(&p, "umadev-phase-architecture").contains(&EvidenceContract::FileContains {
                path: "output/demo-architecture.md".to_string(),
                needle: "API".to_string(),
            })
        );
        assert!(
            ev_of(&p, "umadev-phase-uiux").contains(&EvidenceContract::FileExists {
                path: "output/demo-uiux.md".to_string(),
            })
        );
        // The docs are CHAINED: architecture depends on PRD, UIUX depends on architecture.
        assert_eq!(
            deps_of(&p, "umadev-phase-architecture"),
            &["umadev-phase-prd".to_string()]
        );
        assert_eq!(
            deps_of(&p, "umadev-phase-uiux"),
            &["umadev-phase-architecture".to_string()]
        );
        // The backend code step is wired BEHIND the architect doc (enforce_contract_first),
        // so implementation can't start before the docs land.
        assert!(
            deps_of(&p, "api").contains(&"umadev-phase-architecture".to_string()),
            "the backend step depends on the architect doc: {:?}",
            deps_of(&p, "api")
        );
    }

    #[test]
    fn doc_first_skeleton_does_not_duplicate_a_seated_pm_step() {
        // Idempotence: a brain plan that ALREADY seated the PM keeps that one PM step —
        // the skeleton inserts the MISSING docs (architecture, UIUX) but never a second
        // PM step (matched by seat + build kind), so a doc-first plan is unchanged in
        // its PM seat.
        let p = plan(vec![
            step_seat(
                "my-prd",
                &[],
                Seat::ProductManager,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "api",
                &[],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized(Some(("demo", true)))
        .expect("usable");
        let pm_steps: Vec<&str> = p
            .steps
            .iter()
            .filter(|s| s.seat == Seat::ProductManager && s.kind == StepKind::Build)
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(
            pm_steps,
            vec!["my-prd"],
            "the already-seated PM step is not duplicated: {pm_steps:?}"
        );
        // The synthetic PM id was NOT inserted (the brain's own PM step covers the seat).
        assert!(
            !p.steps.iter().any(|s| s.id == "umadev-phase-prd"),
            "no synthetic PM step when the brain already seated one"
        );
    }

    #[test]
    fn core_doc_floor_is_off_for_a_lean_route_and_never_invents_a_doc_step() {
        // The doc floor fires ONLY on a deliberate route. `normalized(None)` (a lean /
        // quick route) leaves a PM step with NO FileContains doc evidence — the smaller
        // path never demands the full doc set. And a deliberate build whose plan has NO
        // PM/architect step gains NO invented doc evidence (a smaller deliberate change
        // is not forced to author a PRD; finalize reports it missing, not fabricated).
        let lean = plan(vec![step_seat(
            "prd",
            &[],
            Seat::ProductManager,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized(None)
        .expect("usable");
        assert!(
            !ev_of(&lean, "prd").iter().any(
                |c| matches!(c, EvidenceContract::FileContains { path, .. } if path.contains("prd"))
            ),
            "a lean route attaches no PRD FileContains floor: {:?}",
            ev_of(&lean, "prd")
        );
        // Deliberate, but no PM/architect step: a single frontend build step gains its
        // own seat floor (BuildClean) but NO core-doc FileContains — no doc is invented.
        let no_pm = plan(vec![step_seat(
            "ui",
            &[],
            Seat::FrontendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized(Some(("demo", true)))
        .expect("usable");
        assert!(
            !ev_of(&no_pm, "ui")
                .iter()
                .any(|c| matches!(c, EvidenceContract::FileContains { .. })),
            "no PM/architect step ⇒ no invented core-doc evidence: {:?}",
            ev_of(&no_pm, "ui")
        );
    }

    // ── Structural team deliverables: QA test-authoring + designer design-tokens ──

    #[test]
    fn skeleton_inserts_qa_test_authoring_and_design_tokens_steps() {
        // GAP5 / B2#3: "QA writes tests before code" and "designer ships design-tokens"
        // are STRUCTURAL guarantees, not prompt requests. A brain plan with only code
        // steps must, on a deliberate UI-bearing route, gain BOTH prep steps AFTER the
        // doc chain: the QA test-authoring step (authored-tests evidence, NOT a green
        // suite) and the designer tokens step (DesignTokensPresent acceptance) — and
        // the code steps must be wired BEHIND them (test-first / tokens-first).
        let p = plan(vec![
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
        .normalized(Some(("demo", true)))
        .expect("usable");
        // Head order: the three docs, then the code-phase prep
        // (test-plan → design-DIRECTION → design-tokens). UD-CODE-007f: the designer
        // DECIDES a direction before it picks a single value; a hex with no direction
        // behind it is a guess.
        let head: Vec<&str> = p.steps.iter().take(6).map(|s| s.id.as_str()).collect();
        assert_eq!(
            head,
            vec![
                "umadev-phase-prd",
                "umadev-phase-architecture",
                "umadev-phase-uiux",
                "umadev-phase-test-plan",
                "umadev-phase-design-direction",
                "umadev-phase-design-tokens",
            ],
            "docs first, then the QA test-authoring + designer direction + tokens prep"
        );
        // DIRECTION BEFORE TOKENS is a structural edge, not a prompt request.
        let direction = p
            .steps
            .iter()
            .find(|s| s.id == DESIGN_DIRECTION_STEP_ID)
            .expect("the direction step is inserted on a UI-bearing deliberate route");
        assert_eq!(direction.seat, Seat::UiuxDesigner);
        assert!(
            direction
                .evidence
                .contains(&EvidenceContract::FileContains {
                    path: "output/demo-uiux.md".to_string(),
                    needle: "## Visual direction".to_string(),
                }),
            "the direction step's bar is the `## Visual direction` section: {:?}",
            direction.evidence
        );
        assert!(
            is_design_direction_step(direction) && is_design_prep_step(direction),
            "the direction step is designer PREP, never a doc-family anchor"
        );
        // QA test-authoring: evidence = "the authored tests EXIST", never TestPasses
        // (tests are written BEFORE the code they check — a green-suite bar here
        // would deadlock the plan behind its own dependents).
        let qa = p
            .steps
            .iter()
            .find(|s| s.id == "umadev-phase-test-plan")
            .unwrap();
        assert_eq!(qa.seat, Seat::QaEngineer);
        assert_eq!(qa.kind, StepKind::Build);
        assert!(
            qa.evidence.contains(&EvidenceContract::FileExists {
                path: "tests".to_string()
            }),
            "the QA step's bar is authored-tests-exist: {:?}",
            qa.evidence
        );
        assert!(
            !qa.evidence
                .iter()
                .any(|c| matches!(c, EvidenceContract::TestPasses { .. })),
            "the seat floor's TestPasses must NOT bind a test-authoring step: {:?}",
            qa.evidence
        );
        // Test-authoring waits for the docs to be locked (so it runs AFTER the
        // docs_confirm gate, which fires when the doc family completes).
        for d in [
            "umadev-phase-prd",
            "umadev-phase-architecture",
            "umadev-phase-uiux",
        ] {
            assert!(
                qa.depends_on.contains(&d.to_string()),
                "the QA step depends on doc anchor {d}: {:?}",
                qa.depends_on
            );
        }
        // Designer tokens: the bar lives on the AcceptanceSpec axis with EMPTY
        // evidence — a non-empty evidence list would route verification down the
        // evidence path and silently bypass the DesignTokensPresent check.
        let tokens = p
            .steps
            .iter()
            .find(|s| s.id == "umadev-phase-design-tokens")
            .unwrap();
        assert_eq!(tokens.seat, Seat::UiuxDesigner);
        // UD-CODE-007: the bar is CONFORMANCE, not mere existence — the old
        // `DesignTokensPresent` passed on `:root{--color-bg:#000}`.
        assert_eq!(tokens.acceptance, AcceptanceSpec::DesignTokensConform);
        assert!(
            tokens
                .depends_on
                .contains(&DESIGN_DIRECTION_STEP_ID.to_string()),
            "tokens are wired BEHIND the direction step: {:?}",
            tokens.depends_on
        );
        assert!(
            tokens.evidence.is_empty(),
            "the tokens step keeps empty evidence (the acceptance IS the bar; the \
             doc-evidence floor must not bolt the uiux doc onto it): {:?}",
            tokens.evidence
        );
        assert!(is_design_tokens_step(tokens));
        // The uiux DOC step is NOT converted by the designer seat floor — it stays
        // the doc author (FileExists on the uiux doc, weak acceptance untouched).
        let uiux = p
            .steps
            .iter()
            .find(|s| s.id == "umadev-phase-uiux")
            .unwrap();
        assert_eq!(uiux.acceptance, AcceptanceSpec::SourcePresent);
        assert!(!is_design_tokens_step(uiux));
        // TEST-FIRST + TOKENS-FIRST ordering: both code steps build against the
        // authored tests; the frontend additionally waits for the tokens.
        assert!(deps_of(&p, "fe").contains(&"umadev-phase-test-plan".to_string()));
        assert!(deps_of(&p, "be").contains(&"umadev-phase-test-plan".to_string()));
        assert!(deps_of(&p, "fe").contains(&"umadev-phase-design-tokens".to_string()));
        assert!(
            !deps_of(&p, "be").contains(&"umadev-phase-design-tokens".to_string()),
            "only the frontend imports the tokens"
        );
        // The DAG stays schedulable end-to-end (docs → prep → code).
        assert!(!p.ready_steps().is_empty());
    }

    #[test]
    fn skeleton_inserts_test_plan_but_no_tokens_step_on_a_non_ui_build() {
        // needs_ui == false: the QA test-authoring guarantee still holds (test-first
        // is not a UI concern) but no designer tokens step is invented.
        let p = plan(vec![step_seat(
            "api",
            &[],
            Seat::BackendEngineer,
            StepKind::Build,
            AcceptanceSpec::SourcePresent,
        )])
        .normalized(Some(("demo", false)))
        .expect("usable");
        assert!(p.steps.iter().any(|s| s.id == "umadev-phase-test-plan"));
        assert!(
            !p.steps.iter().any(|s| s.id == "umadev-phase-design-tokens"),
            "no tokens step on a UI-less build"
        );
        assert!(deps_of(&p, "api").contains(&"umadev-phase-test-plan".to_string()));
        let qa = p
            .steps
            .iter()
            .find(|s| s.id == "umadev-phase-test-plan")
            .unwrap();
        for d in ["umadev-phase-prd", "umadev-phase-architecture"] {
            assert!(qa.depends_on.contains(&d.to_string()));
        }
    }

    #[test]
    fn skeleton_adopts_a_precode_qa_step_and_a_brain_tokens_step() {
        // Idempotence: a brain plan that ALREADY carries a pre-code QA test-authoring
        // step and a designer tokens step keeps them — no umadev-phase duplicate is
        // inserted; the skeleton only wires the ordering edges. The adopted BARE QA
        // step gains the authored-tests evidence (pre-empting the seat floor's
        // TestPasses, which cannot pass before the code exists).
        let p = plan(vec![
            step_seat(
                "prd",
                &[],
                Seat::ProductManager,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "arch",
                &["prd"],
                Seat::Architect,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "uiux",
                &["arch"],
                Seat::UiuxDesigner,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "qa-author",
                &["arch"],
                Seat::QaEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "tokens",
                &["uiux"],
                Seat::UiuxDesigner,
                StepKind::Build,
                AcceptanceSpec::DesignTokensPresent,
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
        .normalized(Some(("demo", true)))
        .expect("usable");
        assert!(
            !p.steps
                .iter()
                .any(|s| s.id.starts_with("umadev-phase-test-plan")
                    || s.id.starts_with("umadev-phase-design-tokens")
                    || s.id.starts_with("umadev-phase-uiux")),
            "nothing is duplicated when the brain already planned the deliverables: {:?}",
            p.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
        );
        // The adopted bare QA step gained the authored-tests bar, not TestPasses.
        assert!(
            ev_of(&p, "qa-author").contains(&EvidenceContract::FileExists {
                path: "tests".to_string()
            })
        );
        assert!(!ev_of(&p, "qa-author")
            .iter()
            .any(|c| matches!(c, EvidenceContract::TestPasses { .. })));
        // The code steps were wired behind the brain's OWN prep steps.
        for code in ["fe", "be"] {
            assert!(
                deps_of(&p, code).contains(&"qa-author".to_string()),
                "{code} builds against the authored tests: {:?}",
                deps_of(&p, code)
            );
        }
        assert!(deps_of(&p, "fe").contains(&"tokens".to_string()));
        // The adopted tokens step kept its acceptance bar and empty evidence (the
        // doc-evidence floor exempts it — it is NOT the uiux doc author).
        assert!(ev_of(&p, "tokens").is_empty());
    }

    #[test]
    fn skeleton_inserts_its_own_test_plan_when_the_brain_qa_step_needs_a_green_suite() {
        // A brain QA step whose bar REQUIRES passing tests (acceptance build-test, per
        // an older prompt) cannot run before the code exists — adopting it as the
        // test-first anchor would deadlock the plan behind its own dependents. The
        // skeleton inserts its OWN safe test-authoring step instead and wires the code
        // behind THAT; the brain's step is left as planned.
        let p = plan(vec![
            step_seat(
                "qa-green",
                &[],
                Seat::QaEngineer,
                StepKind::Build,
                AcceptanceSpec::BuildTest,
            ),
            step_seat(
                "fe",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized(Some(("demo", true)))
        .expect("usable");
        assert!(
            p.steps.iter().any(|s| s.id == "umadev-phase-test-plan"),
            "a green-suite QA step is not a safe pre-code anchor — insert our own"
        );
        assert!(deps_of(&p, "fe").contains(&"umadev-phase-test-plan".to_string()));
        assert!(
            !deps_of(&p, "fe").contains(&"qa-green".to_string()),
            "the code is never wired behind a bar that needs the code built first"
        );
        // The brain's own step survives untouched (acceptance never downgraded).
        assert_eq!(
            p.steps
                .iter()
                .find(|s| s.id == "qa-green")
                .unwrap()
                .acceptance,
            AcceptanceSpec::BuildTest
        );
    }

    #[test]
    fn tokens_orientation_survives_a_brain_tokens_step_depending_on_code() {
        // A pathological brain tokens step that depends on the frontend it should
        // precede. The skeleton DE-BIASES the adopted tokens step (its code deps are
        // stripped) BEFORE wiring frontend→tokens, so the orientation survives
        // deterministically — never left to an arbitrary cycle-break edge choice.
        let p = plan(vec![
            step_seat(
                "tokens",
                &["fe"],
                Seat::UiuxDesigner,
                StepKind::Build,
                AcceptanceSpec::DesignTokensPresent,
            ),
            step_seat(
                "fe",
                &[],
                Seat::FrontendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized(Some(("demo", true)))
        .expect("usable");
        assert!(
            !deps_of(&p, "tokens").contains(&"fe".to_string()),
            "the tokens step's code dep is stripped (tokens precede the code): {:?}",
            deps_of(&p, "tokens")
        );
        assert!(
            deps_of(&p, "fe").contains(&"tokens".to_string()),
            "the frontend still builds on the tokens: {:?}",
            deps_of(&p, "fe")
        );
    }

    #[test]
    fn designer_build_step_gains_the_design_tokens_floor_when_bare() {
        // The designer arm of the per-seat floor (GAP5): a bare UIUX build step on a
        // lean route is upgraded to the DesignTokensPresent acceptance — the seat's
        // anti-theatre bar (real design-tokens files, not a narrated design system).
        // A frontend peer keeps the plan below the wholesale backstop.
        let p = plan(vec![
            step_seat(
                "design",
                &[],
                Seat::UiuxDesigner,
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
        .normalized(None)
        .expect("usable");
        let design = p.steps.iter().find(|s| s.id == "design").unwrap();
        assert_eq!(
            design.acceptance,
            AcceptanceSpec::DesignTokensPresent,
            "a bare designer build step is judged by its tokens deliverable"
        );
        assert!(
            design.evidence.is_empty(),
            "the designer floor lives on the acceptance axis — no evidence bolted on"
        );
    }

    #[test]
    fn contract_first_orientation_survives_architect_depending_on_an_engineer() {
        // The empirically-reproduced docs-first bypass: the brain planned "document
        // the API after implementation" (architect depends_on the backend step).
        // enforce_contract_first must DE-BIAS the contract author (strip its code
        // deps) BEFORE adding the code→contract edges, so the orientation is
        // deterministic — previously break_dependency_cycles dropped an ARBITRARY
        // back-edge, which could be the contract-first edge itself, letting the
        // backend run before the architecture doc.
        let p = plan(vec![
            step_seat(
                "arch",
                &["be"],
                Seat::Architect,
                StepKind::Build,
                AcceptanceSpec::Contract,
            ),
            step_seat(
                "be",
                &[],
                Seat::BackendEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized(None)
        .expect("usable");
        assert!(
            !deps_of(&p, "arch").contains(&"be".to_string()),
            "the architect's dep on the code it specifies is stripped: {:?}",
            deps_of(&p, "arch")
        );
        assert!(
            deps_of(&p, "be").contains(&"arch".to_string()),
            "the backend depends on the locked contract (orientation survives): {:?}",
            deps_of(&p, "be")
        );
        // And the contract step is what's ready first.
        let ready: Vec<_> = p.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["arch"]);
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
        let p = plan(vec![s]).normalized(None).expect("usable");
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
        .normalized(None)
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
        .normalized(None)
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
        .normalized(None)
        .expect("usable");
        assert!(
            ev_of(&p, "qa-review").is_empty(),
            "a review step carries no synthesized evidence: {:?}",
            ev_of(&p, "qa-review")
        );
    }

    #[test]
    fn falsifiability_backstop_fires_when_most_build_steps_are_bare() {
        // A plan the brain left broadly under-specified: two bare CODE build steps with
        // only the honesty floor. MORE THAN HALF the code-build steps are bare → the
        // backstop synthesizes a deterministic BuildClean bar on each. (Doc-authoring seats
        // — PM/architect/designer — are excluded from this heuristic: they write no code, so
        // BuildClean is unpassable for them; F3.)
        let p = plan(vec![
            step_seat(
                "prd",
                &[],
                Seat::DevopsEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
            step_seat(
                "scope",
                &[],
                Seat::DevopsEngineer,
                StepKind::Build,
                AcceptanceSpec::SourcePresent,
            ),
        ])
        .normalized(None)
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
        .normalized(None)
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

    // ── RED→GREEN evidence contract ────────────────────────────────────────────

    #[test]
    fn red_green_contract_parses_and_holds_an_unnamed_one_as_a_gap() {
        use serde_json::json;
        // A named test parses into the typed contract.
        assert_eq!(
            EvidenceContract::parse_value(
                &json!({"kind":"test-fails-then-passes","test":"login_rejects_bad_password"})
            ),
            Some(EvidenceContract::TestFailsThenPasses {
                test: "login_rejects_bad_password".to_string()
            })
        );
        // `name` is accepted as an alias for `test`, and so are the kind aliases.
        assert_eq!(
            EvidenceContract::parse_value(&json!({"kind":"red-green","name":"t_login"})),
            Some(EvidenceContract::TestFailsThenPasses {
                test: "t_login".to_string()
            })
        );
        // A red→green claim with NO test named is a retained GAP — it must never
        // silently degrade to the coarse "some source exists" default.
        let malformed =
            EvidenceContract::parse_value(&json!({"kind":"test-fails-then-passes"})).unwrap();
        assert!(matches!(malformed, EvidenceContract::Malformed { .. }));
        // There is no bareword form: the contract is meaningless without a test name.
        assert_eq!(
            EvidenceContract::parse_kind_only("test-fails-then-passes"),
            None
        );
    }

    #[test]
    fn red_green_floor_upgrades_a_code_steps_named_test_but_leaves_qa_alone() {
        // THE POINT: on the doc-first path QA authors the tests BEFORE any code, so a
        // code step's named test provably could not have passed beforehand — which
        // makes the red→green bar checkable. Upgrade the code step's weak
        // "test-passes" claim into it. The QA step that AUTHORS the test must NOT be
        // upgraded: its own test is expected to be red at head until the code lands.
        let brain = r#"{"steps":[
          {"id":"qa","title":"author acceptance tests","seat":"qa-engineer","kind":"build",
           "acceptance":"source-present","evidence":[{"kind":"file-exists","path":"tests"}]},
          {"id":"api","title":"build the login route","seat":"backend-engineer","kind":"build",
           "depends_on":["qa"],"acceptance":"build-test",
           "evidence":[{"kind":"test-passes","name":"login_rejects_bad_password"}]}
        ]}"#;
        let raw: BrainPlan = serde_json::from_str(brain).unwrap();
        let plan = Plan {
            steps: raw.steps.into_iter().map(brain_step_to_plan_step).collect(),
            risks: vec![],
            open_questions: vec![],
        }
        .normalized(Some(("demo", false)))
        .expect("plan normalises");

        let api = plan.steps.iter().find(|s| s.id == "api").unwrap();
        assert!(
            api.evidence
                .contains(&EvidenceContract::TestFailsThenPasses {
                    test: "login_rejects_bad_password".to_string()
                }),
            "the implementation step's named-test claim is upgraded to red→green: {:?}",
            api.evidence
        );
        assert!(
            !api.evidence
                .iter()
                .any(|e| matches!(e, EvidenceContract::TestPasses { name: Some(_) })),
            "the weak named-test claim is REPLACED, not duplicated: {:?}",
            api.evidence
        );

        // QA (the test AUTHOR) is untouched — a red→green bar there would demand its
        // own freshly-written test be green at head, which deadlocks the plan.
        let qa = plan.steps.iter().find(|s| s.id == "qa").unwrap();
        assert!(
            !qa.evidence
                .iter()
                .any(|e| matches!(e, EvidenceContract::TestFailsThenPasses { .. })),
            "the test-authoring step never carries a red→green bar: {:?}",
            qa.evidence
        );
    }

    #[test]
    fn red_green_floor_leaves_a_step_that_named_no_test_alone() {
        // OPT-IN, NEVER GLOBAL: a code step whose evidence names no specific test is
        // not conscripted into a red→green bar (there is no test to replay).
        let brain = r#"{"steps":[
          {"id":"qa","title":"author tests","seat":"qa-engineer","kind":"build",
           "acceptance":"source-present","evidence":[{"kind":"file-exists","path":"tests"}]},
          {"id":"api","title":"build it","seat":"backend-engineer","kind":"build",
           "depends_on":["qa"],"acceptance":"build-test","evidence":[{"kind":"build-clean"}]}
        ]}"#;
        let raw: BrainPlan = serde_json::from_str(brain).unwrap();
        let plan = Plan {
            steps: raw.steps.into_iter().map(brain_step_to_plan_step).collect(),
            risks: vec![],
            open_questions: vec![],
        }
        .normalized(Some(("demo", false)))
        .unwrap();
        let api = plan.steps.iter().find(|s| s.id == "api").unwrap();
        assert!(
            !api.evidence
                .iter()
                .any(|e| matches!(e, EvidenceContract::TestFailsThenPasses { .. })),
            "a step that named no test keeps its own bar: {:?}",
            api.evidence
        );
    }

    // ── Declared file surface (the scope-creep denominator) ────────────────────

    #[test]
    fn brain_declared_file_surface_parses_tolerantly() {
        use serde_json::json;
        // Canonical object form.
        let f =
            parse_brain_files(&json!({"create":["src/api/login.ts"],"modify":["src/routes.ts"]}));
        assert_eq!(f.create, vec!["src/api/login.ts"]);
        assert_eq!(f.modify, vec!["src/routes.ts"]);
        assert!(!f.is_empty());

        // A bare array is read as `modify` — the conservative bucket, so a vague
        // declaration can never launder a brand-new file into "claimed as created".
        let bare = parse_brain_files(&json!(["src/a.ts", "src/b.ts"]));
        assert!(bare.create.is_empty());
        assert_eq!(bare.modify.len(), 2);

        // Normalisation: `./`, backslashes, and a leading `/` are stripped.
        let norm = parse_brain_files(&json!({"create":["./src\\api\\x.ts", "/src/y.ts"]}));
        assert_eq!(norm.create, vec!["src/api/x.ts", "src/y.ts"]);

        // A parent-escaping or empty entry is DROPPED — a declaration can only claim
        // paths inside the workspace.
        let escape = parse_brain_files(&json!({"create":["../../etc/passwd", "  ", "src/ok.ts"]}));
        assert_eq!(escape.create, vec!["src/ok.ts"]);

        // Anything unreadable → an EMPTY surface (which disables the check for that
        // step), never a parse failure.
        assert!(parse_brain_files(&json!("src/a.ts")).is_empty());
        assert!(parse_brain_files(&json!(null)).is_empty());
        assert!(parse_brain_files(&json!(7)).is_empty());
    }

    #[test]
    fn skeleton_test_authoring_step_declares_the_surface_it_is_bound_to_write() {
        // The QA step's evidence contract already binds it to deliver tests in
        // `tests/`. Declaring the SAME surface keeps the scope check from reading
        // UmaDev's own structurally-required step as unplanned work.
        let plan = Plan {
            steps: vec![PlanStep {
                files: StepFiles::default(),
                id: "api".into(),
                title: "build the api".into(),
                seat: Seat::BackendEngineer,
                kind: StepKind::Build,
                depends_on: vec![],
                acceptance: AcceptanceSpec::BuildTest,
                evidence: vec![],
                status: StepStatus::Pending,
            }],
            risks: vec![],
            open_questions: vec![],
        }
        .normalized(Some(("demo", false)))
        .unwrap();
        let qa = plan
            .steps
            .iter()
            .find(|s| s.id == "umadev-phase-test-plan")
            .expect("the skeleton seats a QA test-authoring step");
        assert_eq!(
            qa.files.create,
            vec![TEST_AUTHORING_DIR.to_string()],
            "the step declares exactly the surface its evidence binds it to"
        );
    }
}
