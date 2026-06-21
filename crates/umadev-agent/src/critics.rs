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

/// The docs-stage cross-review team, scaled to the task. A lean task gets NO
/// critic team (the deterministic floor + the existing single judge are enough);
/// a heavyweight greenfield / full build gets the PM + architect cross-review.
/// This reuses the planner's complexity tiering (invariant: never MORE ceremony
/// than the task warrants) so a one-line tweak never pays for a review team.
#[must_use]
pub fn docs_team_for_kind(kind: crate::planner::TaskKind) -> Vec<Box<dyn RoleCritic>> {
    use crate::planner::TaskKind;
    match kind {
        // Lean / trivial paths: no cross-review team. The deterministic floor
        // (coverage / contract) plus the existing tech-lead assessment stand.
        TaskKind::Light | TaskKind::Bugfix | TaskKind::Refactor => Vec::new(),
        // Everything that produces real docs gets the docs cross-review team.
        TaskKind::Greenfield
        | TaskKind::FrontendOnly
        | TaskKind::BackendOnly
        | TaskKind::DocsOnly => {
            vec![Box::new(PmCritic), Box::new(ArchitectureCritic)]
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
        // Greenfield / real-doc tasks → PM + architect cross-review team.
        let team = docs_team_for_kind(TaskKind::Greenfield);
        assert_eq!(team.len(), 2);
        let roles: Vec<&str> = team.iter().map(|c| c.role()).collect();
        assert!(roles.contains(&"product-manager"));
        assert!(roles.contains(&"architect"));
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
        };
        let v = PmCritic.review(&stub, arts).await;
        assert_eq!(v.role, "product-manager");
        assert!(!v.accepts);
        assert_eq!(v.blocking, vec!["缺验收标准".to_string()]);
    }
}
