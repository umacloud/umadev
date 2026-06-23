//! Role-critic team layer — makes UmaDev's *implicit* role team explicit.
//!
//! UmaDev already plays several roles in sequence (a PM intake plan, a tech-lead
//! docs assessment, a senior-design review, an acceptance director). Those were
//! ad-hoc one-off judges scattered through the runner. This module gives them a
//! single, uniform shape — a [`RoleVerdict`] schema and a [`RoleCritic`] trait —
//! so a real cross-review *team* can be modelled: each role reviews the shared
//! artifacts from its own seat and returns a structured verdict.
//!
//! HARD INVARIANTS (never break — these are what keep a critic team SAFE):
//!
//! 1. **Fail-open.** A critic that errors, can't be forked, or returns
//!    unparseable output yields an EMPTY verdict ([`RoleVerdict::empty`]) that
//!    `accepts` — it can NEVER block the base. Judgment is an upgrade, never a
//!    dependency (mirrors the `consult` contract).
//! 2. **Deterministic loop control.** A critic verdict is *advisory only*. The
//!    surrounding revision loops stay governed by the deterministic gap-count +
//!    stall-counter floor (coverage / contract / governance). A non-deterministic
//!    LLM verdict must NEVER drive loop termination.
//! 3. **Single-writer / read-only.** A critic NEVER writes files or mutates the
//!    workspace. It reviews artifacts on an ISOLATED forked session (clean,
//!    no-resume) and returns a verdict. Only the main session ever writes.
//! 4. **No new endpoint.** A critic runs over the SAME borrowed brain via the
//!    existing host-driver subprocess (`fork()` + `consult`) — no extra model
//!    endpoint, no extra API key.
//!
//! These constraints are why the team layer is a pure *governance* upgrade: it
//! adds cross-review opinions and an audit trail without ever risking the host.

use serde::{Deserialize, Serialize};

/// A schedulable seat on the delivery team — the unit the router and the plan
/// reason about ("convene a backend engineer"). A seat is a STABLE identity, not a
/// hand-coded heuristic: the same eight roles back the cross-review critic roster
/// (so a [`Seat`] maps 1:1 to a [`RoleCritic`]'s `role()` id) AND name the doers a
/// plan step is assigned to. Doing seats (frontend/backend/…) drive the main
/// session serially under the run-lock; reviewing seats run on read-only forks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Seat {
    /// Product manager — owns scope / requirements / acceptance.
    ProductManager,
    /// Software architect / tech lead — owns the API surface + data model.
    Architect,
    /// UI/UX designer — owns the design system + page hierarchy.
    UiuxDesigner,
    /// Frontend engineer — builds the UI (a doing seat).
    FrontendEngineer,
    /// Backend engineer — builds the API + data layer (a doing seat).
    BackendEngineer,
    /// QA engineer — owns test coverage + the acceptance floor.
    QaEngineer,
    /// Security engineer — owns the attack-surface review.
    SecurityEngineer,
    /// DevOps engineer — owns build / deploy / CI.
    DevopsEngineer,
}

impl Seat {
    /// The stable role id — identical to the matching [`RoleCritic::role`] so a
    /// seat and its critic share ONE id across events, the ledger, and `summon`.
    #[must_use]
    pub const fn role_id(self) -> &'static str {
        match self {
            Self::ProductManager => "product-manager",
            Self::Architect => "architect",
            Self::UiuxDesigner => "uiux-designer",
            Self::FrontendEngineer => "frontend-engineer",
            Self::BackendEngineer => "backend-engineer",
            Self::QaEngineer => "qa-engineer",
            Self::SecurityEngineer => "security-engineer",
            Self::DevopsEngineer => "devops-engineer",
        }
    }

    /// Whether this seat DOES workspace-mutating work (drives the main session
    /// serially) vs only reviews (runs on a read-only fork). The doers are the
    /// engineering seats; the rest are reviewing/advisory seats.
    #[must_use]
    pub const fn is_doer(self) -> bool {
        matches!(
            self,
            Self::FrontendEngineer | Self::BackendEngineer | Self::DevopsEngineer
        )
    }

    /// Resolve a seat from a free-text role id / alias (the same alias set
    /// `director::critic_for_role` accepts, kept in sync). `None` on an unknown
    /// name so a caller fail-opens (an unknown seat is simply ignored).
    #[must_use]
    pub fn from_alias(name: &str) -> Option<Self> {
        match name
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '_'], "-")
            .as_str()
        {
            "product-manager" | "pm" | "product" => Some(Self::ProductManager),
            "architect" | "architecture" | "tech-lead" | "techlead" => Some(Self::Architect),
            "uiux-designer" | "uiux" | "designer" | "ui" | "ux" | "design" => {
                Some(Self::UiuxDesigner)
            }
            "frontend-engineer" | "frontend" | "fe" => Some(Self::FrontendEngineer),
            "backend-engineer" | "backend" | "be" => Some(Self::BackendEngineer),
            "qa-engineer" | "qa" | "test" | "tester" => Some(Self::QaEngineer),
            "security-engineer" | "security" | "sec" => Some(Self::SecurityEngineer),
            "devops-engineer" | "devops" | "sre" | "release" | "ops" => Some(Self::DevopsEngineer),
            _ => None,
        }
    }

    /// The seats a [`crate::planner::TaskKind`] convenes — the roster scaled to the
    /// task. Reuses the planner's complexity sense (a greenfield convenes the full
    /// roster; a bugfix/refactor convenes none — the lean tiers want no team).
    /// Deterministic; the router's reconciliation may widen it.
    #[must_use]
    pub fn team_for_kind(kind: crate::planner::TaskKind) -> Vec<Self> {
        use crate::planner::TaskKind as K;
        match kind {
            K::Greenfield => vec![
                Self::ProductManager,
                Self::Architect,
                Self::UiuxDesigner,
                Self::FrontendEngineer,
                Self::BackendEngineer,
                Self::QaEngineer,
                Self::SecurityEngineer,
            ],
            K::FrontendOnly => vec![
                Self::ProductManager,
                Self::UiuxDesigner,
                Self::FrontendEngineer,
                Self::QaEngineer,
            ],
            K::BackendOnly => vec![
                Self::Architect,
                Self::BackendEngineer,
                Self::QaEngineer,
                Self::SecurityEngineer,
            ],
            // The lean kinds convene no standing team — a bugfix / refactor / trivial
            // build is single-writer + targeted verify, the team is pure overhead.
            K::Bugfix | K::Refactor | K::DocsOnly | K::Light => Vec::new(),
        }
    }
}

/// One role's structured opinion on the shared artifacts — the team layer's
/// unit of cross-review. Aligns with the runner's existing ad-hoc verdicts
/// (`AcceptanceVerdict` / `DocsVerdict` / `DesignVerdict`) but generalises them
/// into ONE shape every role speaks, so a verdict can be recorded, compared, and
/// (for `blocking`) folded into the surrounding deterministic revision loop.
///
/// `accepts` is the role's overall judgement; `blocking` are issues the role
/// considers must-fix (these MAY be fed back as advisory fixes, never as loop
/// control); `advisory` are nice-to-have notes; `evidence` are the concrete
/// observations (file/where) backing the verdict.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RoleVerdict {
    /// The reviewing role (e.g. `product-manager`, `architect`).
    #[serde(default)]
    pub role: String,
    /// Whether the role accepts the artifacts as-is. A partial / failed parse
    /// defaults to `false` only when the model said so; [`RoleVerdict::empty`]
    /// (the fail-open path) sets it to `true` so an absent critic never blocks.
    #[serde(default)]
    pub accepts: bool,
    /// Must-fix issues from this role's seat. Advisory to the loop: they MAY be
    /// folded into the existing revision path, but never govern termination.
    #[serde(default)]
    pub blocking: Vec<String>,
    /// Nice-to-have observations that don't block.
    #[serde(default)]
    pub advisory: Vec<String>,
    /// Concrete observations (file/where) backing the verdict.
    #[serde(default)]
    pub evidence: Vec<String>,
}

impl RoleVerdict {
    /// The fail-open verdict: a named role that ACCEPTS with no findings. This is
    /// what a critic returns when there's no brain, the fork failed, the base
    /// errored, or the reply didn't parse — so an absent / broken critic can
    /// never block the base (invariant 1).
    #[must_use]
    pub fn empty(role: &str) -> Self {
        Self {
            role: role.to_string(),
            accepts: true,
            blocking: Vec::new(),
            advisory: Vec::new(),
            evidence: Vec::new(),
        }
    }

    /// Tag the verdict with its role (the model's JSON usually omits it) and
    /// trim empty entries so the ledger / fix-feedback stay clean.
    #[must_use]
    pub fn normalized(mut self, role: &str) -> Self {
        if self.role.trim().is_empty() {
            self.role = role.to_string();
        }
        let clean = |v: Vec<String>| {
            v.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        };
        self.blocking = clean(self.blocking);
        self.advisory = clean(self.advisory);
        self.evidence = clean(self.evidence);
        self
    }
}

/// What a single role-critic reviews — the shared artifacts handed to the team.
/// Borrowed strings so the runner can assemble a view without cloning whole
/// documents per critic.
///
/// The doc fields (`prd` / `architecture` / `uiux`) feed the DOCS-stage team;
/// the implementation fields (`code` / `qa_floor` / `security_floor`) feed the
/// QUALITY-stage team. Each stage fills only the fields it has — the unused ones
/// stay empty (`Default`), so the same struct serves both stages without forcing
/// a critic to read something that isn't there.
#[derive(Debug, Clone, Copy, Default)]
pub struct CriticArtifacts<'a> {
    /// The original requirement (always present).
    pub requirement: &'a str,
    /// PRD document text (empty when not yet produced).
    pub prd: &'a str,
    /// Architecture document text (empty when not yet produced).
    pub architecture: &'a str,
    /// UI/UX document text (empty when not yet produced).
    pub uiux: &'a str,
    /// Delivered source-code digest (empty at the docs stage — only the
    /// quality-stage team reads it for a semantic review of the real code).
    pub code: &'a str,
    /// The DETERMINISTIC QA-floor findings already computed before the team runs
    /// (uncovered requirements / contract gaps / acceptance gaps). The QA-critic
    /// sees what the hard floor already caught so its semantic pass focuses on
    /// what a deterministic check CAN'T see, not on re-deriving the floor.
    pub qa_floor: &'a str,
    /// The DETERMINISTIC security-floor findings already computed before the team
    /// runs (governance scan / any `security-scan.json`). Same role as
    /// `qa_floor` for the security-critic.
    pub security_floor: &'a str,
}

/// A read-only role on the cross-review team. A critic does NOT act — it reads
/// the shared artifacts from its role's seat and produces a structured
/// [`RoleVerdict`]. It builds the judge prompt; the runner runs it on an
/// ISOLATED forked session via [`CriticConsult`] and never lets the critic
/// touch the workspace (invariant 3).
#[async_trait::async_trait]
pub trait RoleCritic: Send + Sync {
    /// Stable role id (e.g. `product-manager`) — used in the ledger + prompts.
    fn role(&self) -> &str;

    /// Review the shared artifacts and return this role's verdict.
    ///
    /// `consult` runs ONE strict-JSON judge turn on a forked read-only session
    /// and parses it into a [`RoleVerdict`]. It is fail-open: any failure yields
    /// [`RoleVerdict::empty`], so a critic that can't reach the brain ACCEPTS
    /// rather than blocks (invariant 1).
    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict;
}

/// The runner-side capability a critic borrows to think: run one strict-JSON
/// judge prompt on an isolated forked session and parse it into a
/// [`RoleVerdict`]. Object-safe so it can be passed as `&dyn CriticConsult`,
/// keeping critics decoupled from the concrete runtime / runner generics.
///
/// The runner's impl forks a CLEAN read-only session for the judge call, so the
/// critic can never collide with — or write through — the main session.
#[async_trait::async_trait]
pub trait CriticConsult: Send + Sync {
    /// Run a strict-JSON judge turn for `role` and parse it into a verdict.
    /// `system` pins the role + JSON shape; `user` carries the artifacts. Always
    /// returns a verdict — the fail-open empty one when there's no brain / the
    /// call failed / the reply didn't parse.
    async fn judge(&self, role: &str, system: &str, user: String) -> RoleVerdict;
}

/// Product-manager critic — reviews the docs from the PM seat: does the plan
/// actually serve the user + requirement, are scope / acceptance criteria
/// coherent, what's MISSING a user would care about.
pub struct PmCritic;

#[async_trait::async_trait]
impl RoleCritic for PmCritic {
    // The trait returns a borrowed `&str` (general contract); this impl happens to
    // return a literal, but widening it to `&'static str` would diverge from the
    // trait method's signature, so keep the borrowed form.
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "product-manager"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior product manager doing a cross-review of a \
             COMMERCIAL product's plan before the team builds it. From the PM seat, judge \
             whether the PRD actually serves the requirement and the user: clear goal, \
             coherent scope (in/out), testable acceptance criteria that cover the core \
             features, and whether anything a user would care about is MISSING. Only flag \
             REAL gaps; ignore wording nits. JSON shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix gap>\", …], \
             \"advisory\": [\"<nice-to-have>\", …], \"evidence\": [\"<where/why>\", …]}";
        let user = format!(
            "## Requirement\n{}\n\n## PRD\n{}\n\n## Architecture (context)\n{}",
            crate::experts::excerpt(artifacts.requirement, 1200),
            crate::experts::excerpt_sections(artifacts.prd, 5000),
            crate::experts::excerpt_sections(artifacts.architecture, 2000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// Architecture critic — reviews the docs from the architect seat: is the API
/// surface real and complete, is the data model coherent, does the architecture
/// actually cover the PRD's features, are there contract / security gaps.
pub struct ArchitectureCritic;

#[async_trait::async_trait]
impl RoleCritic for ArchitectureCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "architect"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior software architect doing a cross-review of a \
             COMMERCIAL product's plan before the team builds it. From the architect seat, \
             judge whether the architecture is buildable: a real + complete API surface (every \
             core feature has endpoints), a coherent data model, auth / error conventions, and \
             no contract gap between what the PRD promises and what the architecture serves. \
             Only flag REAL gaps; ignore style. JSON shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix gap>\", …], \
             \"advisory\": [\"<nice-to-have>\", …], \"evidence\": [\"<where/why>\", …]}";
        let user = format!(
            "## Requirement\n{}\n\n## Architecture\n{}\n\n## PRD (context)\n{}",
            crate::experts::excerpt(artifacts.requirement, 1200),
            crate::experts::excerpt_sections(artifacts.architecture, 5000),
            crate::experts::excerpt_sections(artifacts.prd, 2000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// QA critic — reviews the DELIVERED code from the QA-engineer seat in the
/// quality stage. The deterministic QA floor (uncovered requirements / contract
/// gaps / acceptance gaps) has ALREADY run as the hard signal before this critic
/// is consulted; the QA-critic's job is the SEMANTIC layer a deterministic check
/// can't reach: do the tests actually exercise the critical paths (not just the
/// happy line), are error / edge / boundary cases handled, is there meaningful
/// coverage of the core feature rather than smoke tests. Advisory only — it
/// NEVER sinks the deterministic quality gate (invariant 2).
pub struct QaCritic;

#[async_trait::async_trait]
impl RoleCritic for QaCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "qa-engineer"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior QA engineer doing a pre-release review of a \
             COMMERCIAL product's DELIVERED code. A deterministic floor already checked \
             requirement coverage / API-contract gaps / acceptance gaps (listed below if \
             any). Your job is the SEMANTIC layer it can't see: do the tests actually \
             exercise the CRITICAL paths (not just the happy line), are error / edge / \
             boundary cases handled, is the core feature meaningfully covered rather than \
             smoke-tested. Only flag REAL test/quality gaps that would ship a broken or \
             untested core path; ignore style. JSON shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix gap>\", …], \
             \"advisory\": [\"<nice-to-have>\", …], \"evidence\": [\"<where/why>\", …]}";
        let floor = if artifacts.qa_floor.trim().is_empty() {
            "(deterministic QA floor: no gaps found)".to_string()
        } else {
            format!(
                "Deterministic QA floor ALREADY flagged (do not just repeat these):\n{}",
                crate::experts::excerpt(artifacts.qa_floor, 1500)
            )
        };
        let user = format!(
            "## Requirement\n{}\n\n## {floor}\n\n## Delivered code (frontend + backend + tests)\n{}",
            crate::experts::excerpt(artifacts.requirement, 1000),
            crate::experts::excerpt(artifacts.code, 16_000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// Security critic — reviews the DELIVERED code from the security-engineer seat
/// in the quality stage. The deterministic security floor (governance scan /
/// any `security-scan.json`) has ALREADY run before this critic is consulted;
/// the security-critic's job is the SEMANTIC attack-surface review a static
/// rule can't make: missing / broken authentication, authorization / IDOR
/// (object-level access) holes, injection surfaces (SQL / command / template),
/// secrets in source, unsafe input handling. Advisory only — it NEVER sinks the
/// deterministic quality gate (invariant 2).
pub struct SecurityCritic;

#[async_trait::async_trait]
impl RoleCritic for SecurityCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "security-engineer"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior application-security engineer doing a \
             pre-release review of a COMMERCIAL product's DELIVERED code. A deterministic \
             governance/security floor already ran (its findings are listed below if any). \
             Your job is the SEMANTIC attack-surface review it can't make: missing or \
             broken AUTHENTICATION, AUTHORIZATION / object-level access (IDOR) holes, \
             INJECTION surfaces (SQL / command / template / XSS), hardcoded secrets, and \
             unsafe input / output handling. Name the file/function and the concrete risk. \
             Only flag REAL exploitable gaps; ignore style. JSON shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix risk>\", …], \
             \"advisory\": [\"<harden later>\", …], \"evidence\": [\"<file/why>\", …]}";
        let floor = if artifacts.security_floor.trim().is_empty() {
            "(deterministic security floor: no violations found)".to_string()
        } else {
            format!(
                "Deterministic security floor ALREADY flagged (do not just repeat these):\n{}",
                crate::experts::excerpt(artifacts.security_floor, 1500)
            )
        };
        let user = format!(
            "## Requirement\n{}\n\n## {floor}\n\n## Delivered code (frontend + backend)\n{}",
            crate::experts::excerpt(artifacts.requirement, 1000),
            crate::experts::excerpt(artifacts.code, 16_000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// UI/UX critic — reviews the docs (and, at the preview gate, the delivered
/// frontend) from the senior product-designer seat. The deterministic governance
/// floor already blocks the obvious mechanical AI-slop tells (emoji-as-icon /
/// hardcoded colors); this critic's job is the SEMANTIC design-quality layer a
/// static rule can't make: is there a real design SYSTEM (a defined token scale —
/// color / type / spacing — not ad-hoc values), is the information architecture
/// and visual hierarchy coherent, are the core component states specified, is the
/// usability sound, and — the taste call — does the UI read as a deliberate
/// commercial product rather than a generic AI-generated template (purple/pink
/// gradient shell, emoji as functional icons, default-font-only, decorative hero
/// over real task flow). Only flag REAL design gaps; ignore wording nits.
pub struct UiuxCritic;

#[async_trait::async_trait]
impl RoleCritic for UiuxCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "uiux-designer"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior product designer doing a cross-review of a \
             COMMERCIAL product's UI/UX before / as the team builds it. From the design seat, \
             judge: (1) is there a real DESIGN SYSTEM — a defined token scale for color, \
             typography and spacing, plus a declared icon library — rather than ad-hoc \
             values; (2) is the information architecture + visual hierarchy coherent and \
             usable; (3) are the core component states specified (default / hover / focus / \
             active / disabled / loading / error); (4) the TASTE call a detector can't make: \
             does it read as a deliberate, on-brand commercial product rather than a generic \
             AI-generated template. BLOCK on: no design-token system, emoji used as \
             functional icons / placeholders, default-system-font-only, a purple/pink-gradient \
             AI-template shell, or a decorative hero standing in for a real task flow. Only \
             flag REAL design gaps; ignore wording nits. JSON shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix design gap>\", …], \
             \"advisory\": [\"<nice-to-have>\", …], \"evidence\": [\"<where/why>\", …]}";
        // Read the UIUX doc as the primary surface; at the preview gate the runner
        // also fills `code`, so the same critic can review the delivered frontend.
        let code_block = if artifacts.code.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n## Delivered frontend (preview)\n{}",
                crate::experts::excerpt(artifacts.code, 12_000)
            )
        };
        let user = format!(
            "## Requirement\n{}\n\n## UI/UX spec\n{}\n\n## PRD (context)\n{}{code_block}",
            crate::experts::excerpt(artifacts.requirement, 1200),
            crate::experts::excerpt_sections(artifacts.uiux, 5000),
            crate::experts::excerpt_sections(artifacts.prd, 1500),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// Frontend-engineering critic — reviews the DELIVERED frontend at the preview
/// gate from the senior front-end engineer seat. The deterministic governance
/// floor already catches emoji / hardcoded-color tells; this critic's job is the
/// SEMANTIC implementation-quality layer: are the interactive component states
/// actually implemented (default / hover / focus / active / disabled / loading /
/// error — not just the resting state), is the UI accessible (semantic markup,
/// labels, keyboard focus, contrast), is it responsive across breakpoints, and —
/// the cross-cutting one — do the frontend `fetch` / `axios` URLs line up exactly
/// with the architecture's API contract (no drifted path / verb). Only flag REAL
/// implementation gaps that would ship a broken or inaccessible UI; ignore style.
pub struct FrontendCritic;

#[async_trait::async_trait]
impl RoleCritic for FrontendCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "frontend-engineer"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior front-end engineer doing a pre-backend review \
             of a COMMERCIAL product's DELIVERED frontend at the preview gate. From the \
             front-end seat, judge the IMPLEMENTATION (not the spec): (1) are the interactive \
             component STATES actually implemented — default / hover / focus / active / \
             disabled / loading / error — not just the resting state; (2) ACCESSIBILITY — \
             semantic markup, form labels, keyboard focus, sufficient contrast; (3) \
             RESPONSIVE layout across breakpoints; (4) FRONTEND↔BACKEND ALIGNMENT — every \
             fetch / axios call hits a path + verb the architecture's API contract actually \
             defines (no drifted or invented endpoint); (5) no hardcoded colors / magic \
             values where a token should be used. Only flag REAL gaps that would ship a \
             broken, inaccessible, or contract-mismatched UI; ignore style. JSON shape: \
             {\"accepts\": <true|false>, \"blocking\": [\"<must-fix gap>\", …], \
             \"advisory\": [\"<nice-to-have>\", …], \"evidence\": [\"<file/why>\", …]}";
        let user = format!(
            "## Requirement\n{}\n\n## UI/UX spec (intent)\n{}\n\n## Architecture API contract (context)\n{}\n\n## Delivered frontend code\n{}",
            crate::experts::excerpt(artifacts.requirement, 1000),
            crate::experts::excerpt_sections(artifacts.uiux, 2000),
            crate::experts::excerpt_sections(artifacts.architecture, 2000),
            crate::experts::excerpt(artifacts.code, 14_000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// Backend-engineering critic — reviews the DELIVERED backend in the quality
/// stage from the senior back-end engineer seat, alongside the QA + security
/// critics. The deterministic QA / contract floor already flags requirement +
/// API-contract gaps (handed in as `qa_floor`); this critic's job is the
/// SEMANTIC server-side engineering layer a deterministic check can't see: is
/// the code properly LAYERED (route / service / data separated, not a monolith
/// in one handler), do the implemented endpoints + data shapes match the
/// architecture's contract, is ERROR HANDLING real (validated input, mapped
/// failures, no swallowed errors), is the security BASELINE present (no obvious
/// injection / missing-auth on a mutating route), and are there glaring
/// ANTI-PATTERNS (N+1 / unbounded query / business logic in the controller).
/// Only flag REAL backend defects; ignore style. Advisory only (invariant 2).
pub struct BackendCritic;

#[async_trait::async_trait]
impl RoleCritic for BackendCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "backend-engineer"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior back-end engineer doing a pre-release review of \
             a COMMERCIAL product's DELIVERED server-side code. A deterministic floor already \
             checked requirement coverage / API-contract gaps (listed below if any). From the \
             back-end seat, judge the SEMANTIC layer it can't see: (1) LAYERING — route / \
             service / data concerns separated, not all stuffed in one handler; (2) CONTRACT \
             FIDELITY — implemented endpoints + request/response shapes match the \
             architecture's API table; (3) ERROR HANDLING — input is validated, failures are \
             mapped to real status codes, nothing is silently swallowed; (4) a SECURITY \
             BASELINE — no obvious injection surface, every mutating route is authorized; \
             (5) no glaring ANTI-PATTERN (N+1 query, unbounded fetch, business logic in the \
             controller). Name the file/function. Only flag REAL defects; ignore style. \
             JSON shape: {\"accepts\": <true|false>, \"blocking\": [\"<must-fix defect>\", …], \
             \"advisory\": [\"<harden later>\", …], \"evidence\": [\"<file/why>\", …]}";
        let floor = if artifacts.qa_floor.trim().is_empty() {
            "(deterministic contract / coverage floor: no gaps found)".to_string()
        } else {
            format!(
                "Deterministic contract / coverage floor ALREADY flagged (do not just repeat these):\n{}",
                crate::experts::excerpt(artifacts.qa_floor, 1500)
            )
        };
        let user = format!(
            "## Requirement\n{}\n\n## {floor}\n\n## Architecture API contract (context)\n{}\n\n## Delivered backend code\n{}",
            crate::experts::excerpt(artifacts.requirement, 1000),
            crate::experts::excerpt_sections(artifacts.architecture, 2500),
            crate::experts::excerpt(artifacts.code, 14_000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// DevOps / release critic — reviews the DELIVERED build in the quality stage
/// from the senior DevOps / SRE seat, alongside the QA and security critics. The
/// deterministic quality gate already ran the project's real build / test / lint
/// (its findings are handed in as `qa_floor`); this critic's job is the SEMANTIC
/// DEPLOYABILITY layer: whether the build / CI actually passes cleanly (no
/// skipped or red step), whether there is RUNTIME EVIDENCE that the app boots and
/// its routes answer (not just that it compiles), whether the deploy TARGET is
/// identifiable (a build script, a start command, and where relevant a container
/// or platform descriptor), whether ENV and SECRETS are externalised (no
/// hardcoded credential or endpoint baked into source — config comes from the
/// environment), and whether the thing is actually release-ready. Only flag REAL
/// ship-blockers; ignore style. Advisory only.
pub struct DevOpsCritic;

#[async_trait::async_trait]
impl RoleCritic for DevOpsCritic {
    #[allow(clippy::unnecessary_literal_bound)]
    fn role(&self) -> &str {
        "devops-engineer"
    }

    async fn review(
        &self,
        consult: &dyn CriticConsult,
        artifacts: CriticArtifacts<'_>,
    ) -> RoleVerdict {
        let system = "You are a STRICT senior DevOps / release engineer doing a pre-ship \
             review of a COMMERCIAL product's DELIVERED build. A deterministic quality gate \
             already ran the project's real build / test / lint (its findings are listed \
             below if any). From the release seat, judge DEPLOYABILITY: (1) does the build / \
             CI pass cleanly — no skipped, missing, or red step; (2) is there RUNTIME \
             EVIDENCE the app boots and its routes answer, not merely that it compiles; \
             (3) is the deploy TARGET identifiable — a build script + start command and, \
             where relevant, a container / platform descriptor; (4) are ENV / SECRETS \
             externalised — no hardcoded credential, token, or environment-specific endpoint \
             baked into source (config comes from the environment); (5) is it actually \
             release-ready. Name the file / step. Only flag REAL ship-blockers; ignore style. \
             JSON shape: {\"accepts\": <true|false>, \"blocking\": [\"<must-fix blocker>\", …], \
             \"advisory\": [\"<harden later>\", …], \"evidence\": [\"<file/step/why>\", …]}";
        let floor = if artifacts.qa_floor.trim().is_empty() {
            "(deterministic build / quality floor: no failures recorded)".to_string()
        } else {
            format!(
                "Deterministic build / quality floor ALREADY recorded (do not just repeat these):\n{}",
                crate::experts::excerpt(artifacts.qa_floor, 1500)
            )
        };
        let user = format!(
            "## Requirement\n{}\n\n## {floor}\n\n## Delivered code (build config + app)\n{}",
            crate::experts::excerpt(artifacts.requirement, 1000),
            crate::experts::excerpt(artifacts.code, 14_000),
        );
        consult.judge(self.role(), system, user).await
    }
}

/// The docs-stage cross-review team, scaled to the task. A lean task gets NO
/// critic team (the deterministic floor + the existing single judge are enough);
/// a heavyweight greenfield / full build gets the PM + architect + designer
/// cross-review. This reuses the planner's complexity tiering (invariant: never
/// MORE ceremony than the task warrants) so a one-line tweak never pays for a
/// review team.
#[must_use]
pub fn docs_team_for_kind(kind: crate::planner::TaskKind) -> Vec<Box<dyn RoleCritic>> {
    use crate::planner::TaskKind;
    match kind {
        // Lean / trivial paths: no cross-review team. The deterministic floor
        // (coverage / contract) plus the existing tech-lead assessment stand.
        TaskKind::Light | TaskKind::Bugfix | TaskKind::Refactor => Vec::new(),
        // A backend-only build produces no UI, so the UIUX seat has nothing to
        // review — the docs team is just PM + architect there.
        TaskKind::BackendOnly => {
            vec![Box::new(PmCritic), Box::new(ArchitectureCritic)]
        }
        // Everything that produces real docs WITH a UI surface gets the full docs
        // cross-review team: PM + architect + UI/UX designer.
        TaskKind::Greenfield | TaskKind::FrontendOnly | TaskKind::DocsOnly => {
            vec![
                Box::new(PmCritic),
                Box::new(ArchitectureCritic),
                Box::new(UiuxCritic),
            ]
        }
    }
}

/// The preview-gate cross-review team, scaled to the task — the THIRD axis of the
/// critic team (docs / preview / quality). After the frontend is built and before
/// the user approves the preview gate, the UI/UX designer + front-end engineer
/// each review the DELIVERED frontend from their own seat. Only the kinds that
/// actually run a frontend phase + preview gate (`Greenfield` / `FrontendOnly`)
/// get a team; everything else has no preview surface to review and gets none.
/// Mirrors the docs / quality tiering so a one-line tweak never pays for a review
/// team.
#[must_use]
pub fn preview_team_for_kind(kind: crate::planner::TaskKind) -> Vec<Box<dyn RoleCritic>> {
    use crate::planner::TaskKind;
    match kind {
        // Only the kinds with a real frontend phase + preview gate get a team.
        TaskKind::Greenfield | TaskKind::FrontendOnly => {
            vec![Box::new(UiuxCritic), Box::new(FrontendCritic)]
        }
        // No preview gate (no frontend phase) → nothing for a preview team.
        TaskKind::BackendOnly
        | TaskKind::DocsOnly
        | TaskKind::Bugfix
        | TaskKind::Refactor
        | TaskKind::Light => Vec::new(),
    }
}

/// The quality-stage cross-review team, scaled to the task — the second axis of
/// the critic team (docs / preview / quality). A lean task gets NO critic team
/// (the deterministic quality floor + the existing single code review are
/// enough); a real build gets the QA + security + DevOps cross-review, and a
/// build with a backend surface also seats the back-end engineer. Mirrors
/// [`docs_team_for_kind`]'s tiering so a one-line tweak never pays for a review
/// team. A `DocsOnly` task produces no code, so it has nothing for a
/// quality-stage team to review and gets none.
#[must_use]
pub fn quality_team_for_kind(kind: crate::planner::TaskKind) -> Vec<Box<dyn RoleCritic>> {
    use crate::planner::TaskKind;
    match kind {
        // Lean / trivial / docs-only paths: no quality cross-review team. The
        // deterministic quality floor plus the existing single code review stand.
        TaskKind::Light | TaskKind::Bugfix | TaskKind::Refactor | TaskKind::DocsOnly => Vec::new(),
        // A frontend-only build has no server layer, so the back-end seat has
        // nothing to review — QA + security + DevOps cover it.
        TaskKind::FrontendOnly => {
            vec![
                Box::new(QaCritic),
                Box::new(SecurityCritic),
                Box::new(DevOpsCritic),
            ]
        }
        // Everything that delivers a backend gets the full quality cross-review
        // team: QA + security + back-end engineer + DevOps.
        TaskKind::Greenfield | TaskKind::BackendOnly => {
            vec![
                Box::new(QaCritic),
                Box::new(SecurityCritic),
                Box::new(BackendCritic),
                Box::new(DevOpsCritic),
            ]
        }
    }
}

/// Append one critic verdict to `.umadev/team-ledger.jsonl` — the team's audit
/// trail, mirroring the existing audit / phase-timing / runs JSONL streams.
/// Records role / accepts / blocking-count / round so a run's cross-review
/// history is inspectable. FAIL-OPEN: any IO error is swallowed; recording a
/// verdict must never affect the run.
pub fn append_team_ledger(
    project_root: &std::path::Path,
    phase: &str,
    round: usize,
    verdict: &RoleVerdict,
) {
    let dir = project_root.join(".umadev");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let entry = serde_json::json!({
        "timestamp": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "phase": phase,
        "round": round,
        "role": verdict.role,
        "accepts": verdict.accepts,
        "blocking": verdict.blocking.len(),
        "advisory": verdict.advisory.len(),
        "evidence": verdict.evidence.len(),
    });
    let path = dir.join("team-ledger.jsonl");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{entry}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_verdict_empty_is_fail_open_accept() {
        // The fail-open verdict must ACCEPT with no findings — an absent critic
        // can never block (invariant 1).
        let v = RoleVerdict::empty("product-manager");
        assert!(v.accepts, "empty verdict must accept (fail-open)");
        assert_eq!(v.role, "product-manager");
        assert!(v.blocking.is_empty());
        assert!(v.advisory.is_empty());
    }

    #[test]
    fn role_verdict_parses_partial_json_and_normalizes() {
        // A partial reply (no role, blanks in lists) still deserializes; then
        // normalized() tags the role and trims empties.
        let json = r#"{"accepts": false, "blocking": ["缺登录", "  "], "evidence": ["prd.md"]}"#;
        let v: RoleVerdict = serde_json::from_str(json).unwrap();
        let v = v.normalized("architect");
        assert_eq!(v.role, "architect", "missing role is tagged on normalize");
        assert!(!v.accepts);
        assert_eq!(v.blocking, vec!["缺登录".to_string()], "blanks trimmed");
        assert_eq!(v.evidence, vec!["prd.md".to_string()]);
    }

    #[test]
    fn append_team_ledger_writes_jsonl_and_is_fail_open() {
        let tmp = tempfile::TempDir::new().unwrap();
        let v = RoleVerdict {
            role: "product-manager".into(),
            accepts: false,
            blocking: vec!["a".into(), "b".into()],
            advisory: vec!["c".into()],
            evidence: vec![],
        };
        append_team_ledger(tmp.path(), "docs", 1, &v);
        let content =
            std::fs::read_to_string(tmp.path().join(".umadev/team-ledger.jsonl")).unwrap();
        assert!(content.contains("\"role\":\"product-manager\""));
        assert!(content.contains("\"blocking\":2"));
        assert!(content.contains("\"round\":1"));
        // A second append accumulates (append mode, not truncate).
        append_team_ledger(tmp.path(), "docs", 1, &v);
        let lines = std::fs::read_to_string(tmp.path().join(".umadev/team-ledger.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(lines, 2);
    }

    #[test]
    fn docs_team_scales_with_task_kind() {
        use crate::planner::TaskKind;
        // Lean / trivial → NO critic team (deterministic floor stands).
        assert!(docs_team_for_kind(TaskKind::Light).is_empty());
        assert!(docs_team_for_kind(TaskKind::Bugfix).is_empty());
        assert!(docs_team_for_kind(TaskKind::Refactor).is_empty());
        // Greenfield / UI-bearing doc tasks → PM + architect + UI/UX designer.
        let team = docs_team_for_kind(TaskKind::Greenfield);
        assert_eq!(team.len(), 3);
        let roles: Vec<&str> = team.iter().map(|c| c.role()).collect();
        assert!(roles.contains(&"product-manager"));
        assert!(roles.contains(&"architect"));
        assert!(roles.contains(&"uiux-designer"));
        // Frontend-only + docs-only also have a UI surface → designer seated.
        assert_eq!(docs_team_for_kind(TaskKind::FrontendOnly).len(), 3);
        assert_eq!(docs_team_for_kind(TaskKind::DocsOnly).len(), 3);
        // Backend-only produces no UI → designer NOT seated (PM + architect only).
        let be = docs_team_for_kind(TaskKind::BackendOnly);
        assert_eq!(be.len(), 2);
        let be_roles: Vec<&str> = be.iter().map(|c| c.role()).collect();
        assert!(!be_roles.contains(&"uiux-designer"));
    }

    #[test]
    fn preview_team_scales_with_task_kind() {
        use crate::planner::TaskKind;
        // Kinds with a real frontend phase + preview gate → UIUX + frontend.
        for k in [TaskKind::Greenfield, TaskKind::FrontendOnly] {
            let team = preview_team_for_kind(k);
            assert_eq!(team.len(), 2, "{k:?} seats the preview team");
            let roles: Vec<&str> = team.iter().map(|c| c.role()).collect();
            assert!(roles.contains(&"uiux-designer"));
            assert!(roles.contains(&"frontend-engineer"));
        }
        // No frontend phase / preview gate → no preview team.
        assert!(preview_team_for_kind(TaskKind::BackendOnly).is_empty());
        assert!(preview_team_for_kind(TaskKind::DocsOnly).is_empty());
        assert!(preview_team_for_kind(TaskKind::Bugfix).is_empty());
        assert!(preview_team_for_kind(TaskKind::Refactor).is_empty());
        assert!(preview_team_for_kind(TaskKind::Light).is_empty());
    }

    /// A stub consult that returns a fixed verdict — proves a critic's review()
    /// builds a prompt and threads the verdict through without a real runtime.
    struct StubConsult(RoleVerdict);

    #[async_trait::async_trait]
    impl CriticConsult for StubConsult {
        async fn judge(&self, role: &str, _system: &str, _user: String) -> RoleVerdict {
            self.0.clone().normalized(role)
        }
    }

    #[tokio::test]
    async fn pm_critic_review_threads_verdict() {
        let stub = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["缺验收标准".into()],
            ..Default::default()
        });
        let arts = CriticArtifacts {
            requirement: "做一个登录系统",
            prd: "# PRD\n## Goal\n登录",
            architecture: "",
            uiux: "",
            ..Default::default()
        };
        let v = PmCritic.review(&stub, arts).await;
        assert_eq!(v.role, "product-manager");
        assert!(!v.accepts);
        assert_eq!(v.blocking, vec!["缺验收标准".to_string()]);
    }

    #[test]
    fn quality_team_scales_with_task_kind() {
        use crate::planner::TaskKind;
        // Lean / trivial / docs-only → NO quality team (deterministic floor stands).
        assert!(quality_team_for_kind(TaskKind::Light).is_empty());
        assert!(quality_team_for_kind(TaskKind::Bugfix).is_empty());
        assert!(quality_team_for_kind(TaskKind::Refactor).is_empty());
        assert!(
            quality_team_for_kind(TaskKind::DocsOnly).is_empty(),
            "docs-only delivers no code → nothing for a quality team to review"
        );
        // Greenfield ships a full stack → QA + security + backend + DevOps.
        let team = quality_team_for_kind(TaskKind::Greenfield);
        assert_eq!(team.len(), 4);
        let roles: Vec<&str> = team.iter().map(|c| c.role()).collect();
        assert!(roles.contains(&"qa-engineer"));
        assert!(roles.contains(&"security-engineer"));
        assert!(roles.contains(&"backend-engineer"));
        assert!(roles.contains(&"devops-engineer"));
        // Backend-only also ships a server → same full team.
        assert_eq!(quality_team_for_kind(TaskKind::BackendOnly).len(), 4);
        // Frontend-only has no server layer → backend seat dropped (QA + security
        // + DevOps).
        let fe = quality_team_for_kind(TaskKind::FrontendOnly);
        assert_eq!(fe.len(), 3);
        let fe_roles: Vec<&str> = fe.iter().map(|c| c.role()).collect();
        assert!(!fe_roles.contains(&"backend-engineer"));
        assert!(fe_roles.contains(&"devops-engineer"));
    }

    #[tokio::test]
    async fn qa_critic_review_threads_verdict() {
        // The QA-critic builds its prompt from the code + deterministic floor and
        // threads the verdict through, tagged with the qa-engineer role.
        let stub = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["登录失败路径无测试".into()],
            evidence: vec!["auth.test.ts".into()],
            ..Default::default()
        });
        let arts = CriticArtifacts {
            requirement: "做一个登录系统",
            code: "// auth.ts\nfn login() {}",
            qa_floor: "FR-002 注销 无任务覆盖",
            ..Default::default()
        };
        let v = QaCritic.review(&stub, arts).await;
        assert_eq!(v.role, "qa-engineer");
        assert!(!v.accepts);
        assert_eq!(v.blocking, vec!["登录失败路径无测试".to_string()]);
        assert_eq!(v.evidence, vec!["auth.test.ts".to_string()]);
    }

    #[tokio::test]
    async fn security_critic_review_threads_verdict() {
        // The security-critic builds its prompt from the code + deterministic
        // floor and threads the verdict through, tagged with the security role.
        let stub = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["DELETE /api/todos/:id 无鉴权(IDOR)".into()],
            ..Default::default()
        });
        let arts = CriticArtifacts {
            requirement: "做一个待办系统",
            code: "// api.ts\napp.delete('/api/todos/:id', handler)",
            security_floor: "",
            ..Default::default()
        };
        let v = SecurityCritic.review(&stub, arts).await;
        assert_eq!(v.role, "security-engineer");
        assert!(!v.accepts);
        assert_eq!(
            v.blocking,
            vec!["DELETE /api/todos/:id 无鉴权(IDOR)".to_string()]
        );
    }

    // ---- The four newer seats: name + an ACCEPT case + a blocking case. The
    // StubConsult returns whatever verdict it was built with (tagged with the
    // critic's role on normalize), so an accepting stub exercises the ACCEPT path
    // and a blocking stub exercises the must-fix path — both prove the critic
    // builds its prompt and threads the verdict through without a real runtime. ----

    #[tokio::test]
    async fn uiux_critic_name_accept_and_block() {
        assert_eq!(UiuxCritic.role(), "uiux-designer");
        let arts = CriticArtifacts {
            requirement: "做一个 SaaS 仪表盘",
            uiux: "# UIUX\n## Design tokens\n- color/type/spacing scale\n## Component states",
            prd: "# PRD\n仪表盘",
            ..Default::default()
        };
        // ACCEPT case: a clean design system → accepts, no blocking.
        let ok = StubConsult(RoleVerdict {
            accepts: true,
            ..Default::default()
        });
        let v = UiuxCritic.review(&ok, arts).await;
        assert_eq!(v.role, "uiux-designer");
        assert!(v.accepts && v.blocking.is_empty());
        // BLOCKING case: AI-slop / no token system → blocks.
        let bad = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["无设计令牌系统,用 emoji 当功能图标(AI 模板感)".into()],
            evidence: vec!["uiux".into()],
            ..Default::default()
        });
        let v = UiuxCritic.review(&bad, arts).await;
        assert_eq!(v.role, "uiux-designer");
        assert!(!v.accepts);
        assert_eq!(
            v.blocking,
            vec!["无设计令牌系统,用 emoji 当功能图标(AI 模板感)".to_string()]
        );
    }

    #[tokio::test]
    async fn frontend_critic_name_accept_and_block() {
        assert_eq!(FrontendCritic.role(), "frontend-engineer");
        let arts = CriticArtifacts {
            requirement: "做一个登录页",
            uiux: "# UIUX\n登录表单",
            architecture: "## API\n| POST | /api/login |",
            code: "// LoginForm.tsx\nfetch('/api/login')",
            ..Default::default()
        };
        // ACCEPT case.
        let ok = StubConsult(RoleVerdict {
            accepts: true,
            ..Default::default()
        });
        let v = FrontendCritic.review(&ok, arts).await;
        assert_eq!(v.role, "frontend-engineer");
        assert!(v.accepts && v.blocking.is_empty());
        // BLOCKING case: missing states + contract drift.
        let bad = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec![
                "按钮无 disabled/loading 态;fetch('/api/signin') 与契约 /api/login 不一致".into(),
            ],
            evidence: vec!["LoginForm.tsx".into()],
            ..Default::default()
        });
        let v = FrontendCritic.review(&bad, arts).await;
        assert_eq!(v.role, "frontend-engineer");
        assert!(!v.accepts);
        assert_eq!(v.blocking.len(), 1);
        assert!(v.blocking[0].contains("disabled/loading"));
    }

    #[tokio::test]
    async fn backend_critic_name_accept_and_block() {
        assert_eq!(BackendCritic.role(), "backend-engineer");
        let arts = CriticArtifacts {
            requirement: "做一个待办后端",
            architecture: "## API\n| GET | /api/todos |",
            code: "// todos.ts\nrouter.get('/api/todos', svc.list)",
            qa_floor: "",
            ..Default::default()
        };
        // ACCEPT case (clean floor → designer-style accepting verdict).
        let ok = StubConsult(RoleVerdict {
            accepts: true,
            ..Default::default()
        });
        let v = BackendCritic.review(&ok, arts).await;
        assert_eq!(v.role, "backend-engineer");
        assert!(v.accepts && v.blocking.is_empty());
        // BLOCKING case: layering + error-handling defect.
        let bad = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["业务逻辑写在 controller,无输入校验/错误映射".into()],
            evidence: vec!["todos.ts".into()],
            ..Default::default()
        });
        let v = BackendCritic.review(&bad, arts).await;
        assert_eq!(v.role, "backend-engineer");
        assert!(!v.accepts);
        assert_eq!(
            v.blocking,
            vec!["业务逻辑写在 controller,无输入校验/错误映射".to_string()]
        );
    }

    #[tokio::test]
    async fn devops_critic_name_accept_and_block() {
        assert_eq!(DevOpsCritic.role(), "devops-engineer");
        let arts = CriticArtifacts {
            requirement: "上线一个 web 服务",
            code: "// server.ts\nconst PORT = process.env.PORT",
            qa_floor: "build: ok\ntest: ok",
            ..Default::default()
        };
        // ACCEPT case: build green, env externalised.
        let ok = StubConsult(RoleVerdict {
            accepts: true,
            ..Default::default()
        });
        let v = DevOpsCritic.review(&ok, arts).await;
        assert_eq!(v.role, "devops-engineer");
        assert!(v.accepts && v.blocking.is_empty());
        // BLOCKING case: hardcoded secret + no runtime proof.
        let bad = StubConsult(RoleVerdict {
            accepts: false,
            blocking: vec!["源码中硬编码数据库密钥;无运行时启动证据".into()],
            evidence: vec!["server.ts".into()],
            ..Default::default()
        });
        let v = DevOpsCritic.review(&bad, arts).await;
        assert_eq!(v.role, "devops-engineer");
        assert!(!v.accepts);
        assert!(v.blocking[0].contains("硬编码"));
    }
}
