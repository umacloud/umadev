//! Executable scope contract for one routed turn or owned delivery plan.
//!
//! Intent classification answers *what the user meant*. It does not by itself
//! constrain what a capable base may touch after it starts working. This module
//! closes that second boundary with an owned, inspectable contract:
//!
//! - the latest objective is the only authority;
//! - every mutating plan step declares the files/directories it may create or edit;
//! - high-risk surfaces (dependencies, CI, migrations, credentials) require an
//!   explicit path claim;
//! - the number of changed files is bounded proportionally to the routed task; and
//! - deliverables and verification remain visible beside the allowed surface.
//!
//! The contract is deliberately independent of any provider protocol. Drivers may
//! use it to pre-authorise a tool when the base exposes that capability; every path
//! also uses it as a deterministic post-condition over the workspace diff.

use std::collections::BTreeSet;

use crate::plan_state::{AcceptanceSpec, EvidenceContract, Plan, PlanStep, StepKind};
use crate::router::{Depth, RouteClass, RoutePlan};

/// A deterministic contract violation. Every value is blocking: advisory scope
/// observations remain in [`crate::scope_creep`], while this type represents a
/// boundary the delivery must satisfy before it can be called complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractViolation {
    /// Stable machine-readable category.
    pub code: &'static str,
    /// Evidence-bearing user/developer explanation.
    pub message: String,
    /// Workspace-relative path involved, when the violation concerns one file.
    pub path: Option<String>,
}

/// The owned execution boundary for a mutating route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionContract {
    /// The current request, retained for audit and prompt rendering.
    pub objective: String,
    /// Workspace-relative paths or directory/glob claims the run may change.
    pub allowed_paths: Vec<String>,
    /// Concrete file deliverables extracted from typed evidence.
    pub deliverables: Vec<String>,
    /// Human-readable mechanical verification obligations.
    pub verification: Vec<String>,
    /// Hard changed-file budget for this route.
    pub max_changed_files: usize,
    /// Mutating plan steps whose file surface is absent.
    pub missing_surface_steps: Vec<String>,
}

impl ExecutionContract {
    /// Build the lightweight contract used before a plan exists (chat-resident
    /// QuickEdit/Fast Debug and the lean Build lane). Model-supplied scope is treated
    /// as an allow-list only when it contains valid workspace-relative claims. An
    /// empty scope does not invent paths, but sensitive surfaces still require an
    /// explicit claim and the changed-file budget remains active.
    #[must_use]
    pub fn from_route(route: &RoutePlan, objective: &str) -> Self {
        let allowed_paths = normalized_unique(route.scope.iter().map(String::as_str));
        Self {
            objective: objective.trim().to_string(),
            allowed_paths,
            deliverables: Vec::new(),
            verification: route_verification(route),
            max_changed_files: route_change_budget(route),
            missing_surface_steps: Vec::new(),
        }
    }

    /// Build the strict contract for an owned plan. The plan's declared surfaces,
    /// not the router's advisory hints, are authoritative. Every Build step must
    /// contribute a surface; missing declarations are explicit preflight failures
    /// instead of silently disabling scope enforcement for the whole run.
    #[must_use]
    pub fn from_plan(route: &RoutePlan, objective: &str, plan: &Plan) -> Self {
        let allowed_paths = normalized_unique(plan.steps.iter().flat_map(|step| step.files.all()));
        let mut deliverables = BTreeSet::new();
        let mut verification = BTreeSet::new();
        let mut missing_surface_steps = Vec::new();
        for step in &plan.steps {
            if step.kind == StepKind::Build && step.files.is_empty() {
                missing_surface_steps.push(format!("{} · {}", step.id, step.title));
            }
            verification.insert(format!("{}: {}", step.id, step.criterion_label()));
            for evidence in &step.evidence {
                if let EvidenceContract::FileExists { path }
                | EvidenceContract::FileContains { path, .. } = evidence
                {
                    if let Some(path) = normalize_claim(path) {
                        deliverables.insert(path);
                    }
                }
            }
            if matches!(
                step.acceptance,
                AcceptanceSpec::DesignTokensPresent | AcceptanceSpec::DesignTokensConform
            ) {
                deliverables.insert("design-tokens.{json,css}".to_string());
            }
        }
        let exact_claims = allowed_paths
            .iter()
            .filter(|path| !path.contains('*') && !path.ends_with('/'))
            .count();
        Self {
            objective: objective.trim().to_string(),
            allowed_paths,
            deliverables: deliverables.into_iter().collect(),
            verification: verification.into_iter().collect(),
            max_changed_files: route_change_budget(route).max(exact_claims),
            missing_surface_steps,
        }
    }

    /// Build the narrow contract injected into one scheduled Build step. Only that
    /// step's declared files are allowed, even when later steps in the same plan will
    /// legitimately touch other parts of the repository.
    #[must_use]
    pub fn from_step(route: &RoutePlan, objective: &str, step: &PlanStep) -> Self {
        let mut contract = Self::from_plan(
            route,
            objective,
            &Plan {
                steps: vec![step.clone()],
                risks: Vec::new(),
                open_questions: Vec::new(),
            },
        );
        contract.max_changed_files = step_change_budget(&contract.allowed_paths)
            .min(contract.max_changed_files)
            .max(contract.allowed_paths.len());
        contract
    }

    /// Pre-execution failures. A plan with an incomplete write denominator cannot
    /// safely enforce scope, so it must be repaired/re-planned before a writer runs.
    #[must_use]
    pub fn preflight_violations(&self) -> Vec<ContractViolation> {
        if self.missing_surface_steps.is_empty() {
            return Vec::new();
        }
        vec![ContractViolation {
            code: "execution-contract-incomplete",
            message: format!(
                "execution contract is incomplete: mutating plan step(s) declared no file surface: {}. Re-plan with explicit `files.create`/`files.modify`; do not start an unconstrained writer",
                self.missing_surface_steps.join("; ")
            ),
            path: None,
        }]
    }

    /// Validate the paths actually changed by a turn/run. Inputs are expected to be
    /// workspace-relative diff paths; malformed/absolute/parent-escaping values are
    /// rejected rather than normalised into an in-workspace claim.
    #[must_use]
    pub fn validate_changed_paths<'a>(
        &self,
        changed: impl IntoIterator<Item = &'a str>,
    ) -> Vec<ContractViolation> {
        let mut paths = BTreeSet::new();
        let mut out = Vec::new();
        for raw in changed {
            let Some(path) = normalize_changed_path(raw) else {
                out.push(ContractViolation {
                    code: "execution-path-invalid",
                    message: format!(
                        "execution contract rejected a non-workspace or malformed changed path: `{raw}`"
                    ),
                    path: Some(raw.to_string()),
                });
                continue;
            };
            if is_internal_runtime_path(&path) {
                continue;
            }
            paths.insert(path);
        }

        if paths.len() > self.max_changed_files {
            out.push(ContractViolation {
                code: "execution-change-budget-exceeded",
                message: format!(
                    "execution contract allows at most {} changed file(s), but this turn/run changed {}. Split or re-plan the work before continuing",
                    self.max_changed_files,
                    paths.len()
                ),
                path: None,
            });
        }

        for path in paths {
            let explicitly_allowed = self
                .allowed_paths
                .iter()
                .any(|claim| claim_covers(claim, &path));
            let outside_declared_plan = !self.allowed_paths.is_empty() && !explicitly_allowed;
            let undeclared_sensitive =
                self.allowed_paths.is_empty() && is_sensitive_surface(&path) && !explicitly_allowed;
            if outside_declared_plan || undeclared_sensitive {
                let why = if undeclared_sensitive {
                    "a dependency/CI/migration/credential surface requires an explicit scope claim"
                } else {
                    "no plan step or routed scope claimed this path"
                };
                out.push(ContractViolation {
                    code: "execution-path-out-of-scope",
                    message: format!(
                        "execution contract rejected `{path}`: {why}. Remove the change or explicitly re-plan it before continuing"
                    ),
                    path: Some(path),
                });
            }
        }
        out
    }

    /// A compact, provider-neutral instruction block for a step/turn. This is the
    /// model-facing belt; [`Self::validate_changed_paths`] is the deterministic
    /// post-condition that prevents prompt compliance from being the only boundary.
    #[must_use]
    pub fn prompt_block(&self) -> String {
        let paths = if self.allowed_paths.is_empty() {
            "- No exact path could be derived before this lightweight turn. Discover the minimum ordinary source surface needed for the objective; do not touch dependencies, CI/release, migrations, credentials, or unrelated files."
                .to_string()
        } else {
            self.allowed_paths
                .iter()
                .map(|path| format!("- `{path}`"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let deliverables = if self.deliverables.is_empty() {
            "- Use the step's typed acceptance/evidence exactly.".to_string()
        } else {
            self.deliverables
                .iter()
                .map(|path| format!("- `{path}`"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        format!(
            "## Execution contract (hard boundary)\n\
             Objective: {}\n\
             Allowed create/modify surface:\n{}\n\
             Required deliverables:\n{}\n\
             Change budget: at most {} file(s).\n\
             Forbidden unless explicitly re-planned: every other file, opportunistic refactors, new dependencies, CI/release changes, migrations, credentials/secrets, and unrelated governance/review work. Stop when the declared acceptance passes.",
            self.objective,
            paths,
            deliverables,
            self.max_changed_files,
        )
    }
}

/// Populate a step's file surface from exact file evidence when the planner omitted
/// the redundant `files` object. This is a lossless derivation: the verifier already
/// requires that same path, so declaring it cannot widen the step. Design-token
/// acceptance has a fixed filename contract and therefore gains its three supported
/// locations. A generic BuildClean/SourcePresent claim remains empty and must be
/// repaired by the planning turn rather than guessed.
pub(crate) fn infer_step_surface(step: &mut PlanStep) {
    if !step.files.is_empty() || step.kind != StepKind::Build {
        return;
    }
    for evidence in &step.evidence {
        if let EvidenceContract::FileExists { path } | EvidenceContract::FileContains { path, .. } =
            evidence
        {
            if let Some(path) = normalize_claim(path) {
                step.files.create.push(path);
            }
        }
    }
    if step.files.is_empty()
        && matches!(
            step.acceptance,
            AcceptanceSpec::DesignTokensPresent | AcceptanceSpec::DesignTokensConform
        )
    {
        step.files.create.extend([
            "design-tokens.json".to_string(),
            "design-tokens.css".to_string(),
            "src/styles/design-tokens.css".to_string(),
        ]);
    }
    step.files.create.sort();
    step.files.create.dedup();
}

fn route_verification(route: &RoutePlan) -> Vec<String> {
    match route.class {
        RouteClass::Chat | RouteClass::Explain => Vec::new(),
        RouteClass::QuickEdit => vec!["targeted verification after the last write".to_string()],
        RouteClass::Debug => vec!["targeted regression verification after the fix".to_string()],
        RouteClass::Build => vec!["owned plan acceptance plus final whole-build QC".to_string()],
    }
}

fn route_change_budget(route: &RoutePlan) -> usize {
    match (route.class, route.depth) {
        (RouteClass::Chat | RouteClass::Explain, _) => 0,
        (RouteClass::QuickEdit, _) => 4,
        (RouteClass::Debug, Depth::Fast) => 8,
        (RouteClass::Debug, _) => 64,
        (RouteClass::Build, Depth::Fast) => 24,
        (RouteClass::Build, Depth::Standard) => 160,
        (RouteClass::Build, Depth::Deep) => 400,
    }
}

fn step_change_budget(claims: &[String]) -> usize {
    claims.iter().fold(0usize, |budget, claim| {
        let leaf = claim.rsplit('/').next().unwrap_or(claim);
        if claim.contains('*') || claim.ends_with('/') || !leaf.contains('.') {
            budget.saturating_add(16)
        } else {
            budget.saturating_add(1)
        }
    })
}

fn normalized_unique<'a>(items: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    items
        .into_iter()
        .filter_map(normalize_claim)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_claim(raw: &str) -> Option<String> {
    let path = raw.trim().trim_matches(['`', '"', '\'']).replace('\\', "/");
    if path.starts_with('/') || path.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    let path = path.strip_prefix("./").unwrap_or(&path);
    if path.is_empty()
        || path == "."
        || path.bytes().all(|byte| byte == b'*')
        || path.contains(char::is_control)
        || path.contains(':')
        || path.contains("//")
        || path.split('/').any(|segment| segment == "..")
    {
        return None;
    }
    Some(path.to_string())
}

fn normalize_changed_path(raw: &str) -> Option<String> {
    let path = raw.trim().replace('\\', "/");
    if path.starts_with('/')
        || path.as_bytes().get(1) == Some(&b':')
        || path.split('/').any(|segment| segment == "..")
    {
        return None;
    }
    normalize_claim(&path)
}

fn claim_covers(claim: &str, path: &str) -> bool {
    let claim = claim.to_ascii_lowercase();
    let path = path.to_ascii_lowercase();
    if claim.contains('*') {
        return wildcard_match(claim.as_bytes(), path.as_bytes());
    }
    let directory = claim.trim_end_matches('/');
    path == directory || path.starts_with(&format!("{directory}/"))
}

/// Glob match where `*` matches any run of bytes, `/` included (so `**` acts
/// the same). Callers lowercase both sides for case-insensitive matching.
pub(crate) fn wildcard_match(pattern: &[u8], value: &[u8]) -> bool {
    let (mut p, mut v, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = v;
        } else if let Some(s) = star {
            p = s + 1;
            mark += 1;
            v = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn is_internal_runtime_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower == ".umadev" || lower.starts_with(".umadev/")
}

fn is_sensitive_surface(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower == ".git"
        || lower.starts_with(".git/")
        || lower.starts_with(".github/workflows/")
        || lower.starts_with(".circleci/")
        || lower.contains("/migrations/")
        || lower.starts_with("migrations/")
        || name.starts_with(".env")
        || matches!(
            name,
            "package.json"
                | "cargo.toml"
                | "go.mod"
                | "pyproject.toml"
                | "requirements.txt"
                | "pom.xml"
                | "build.gradle"
                | "build.gradle.kts"
                | "composer.json"
                | "gemfile"
                | "dockerfile"
                | ".gitlab-ci.yml"
                | "azure-pipelines.yml"
                | "security.md"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::plan_state::{StepFiles, StepStatus};
    use crate::planner::TaskKind;
    use crate::router::Budget;

    fn route(class: RouteClass, depth: Depth, scope: &[&str]) -> RoutePlan {
        RoutePlan {
            class,
            kind: TaskKind::Light,
            depth,
            team: Vec::new(),
            scope: scope.iter().map(|s| (*s).to_string()).collect(),
            needs_clarify: None,
            est_budget: Budget::for_route(class, depth),
            confidence: 1.0,
        }
    }

    fn step(id: &str, files: &[&str]) -> PlanStep {
        PlanStep {
            id: id.to_string(),
            title: id.to_string(),
            seat: Seat::BackendEngineer,
            kind: StepKind::Build,
            depends_on: Vec::new(),
            acceptance: AcceptanceSpec::BuildTest,
            evidence: Vec::new(),
            files: StepFiles {
                create: files.iter().map(|s| (*s).to_string()).collect(),
                modify: Vec::new(),
            },
            status: StepStatus::Pending,
        }
    }

    #[test]
    fn missing_plan_surface_is_an_explicit_preflight_failure() {
        let plan = Plan {
            steps: vec![step("backend", &[])],
            risks: Vec::new(),
            open_questions: Vec::new(),
        };
        let contract = ExecutionContract::from_plan(
            &route(RouteClass::Build, Depth::Standard, &[]),
            "add SEO metadata",
            &plan,
        );
        let violations = contract.preflight_violations();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "execution-contract-incomplete");
        assert!(violations[0].message.contains("backend"));
    }

    #[test]
    fn exact_evidence_infers_the_same_allowed_surface() {
        let mut step = step("seo", &[]);
        step.evidence = vec![EvidenceContract::FileContains {
            path: "src/app/layout.tsx".to_string(),
            needle: "metadata".to_string(),
        }];
        infer_step_surface(&mut step);
        assert_eq!(step.files.create, ["src/app/layout.tsx"]);
    }

    #[test]
    fn undeclared_edits_are_blocking_not_advisory() {
        let plan = Plan {
            steps: vec![step("seo", &["src/app/layout.tsx"])],
            risks: Vec::new(),
            open_questions: Vec::new(),
        };
        let contract = ExecutionContract::from_plan(
            &route(RouteClass::Build, Depth::Standard, &[]),
            "add SEO metadata",
            &plan,
        );
        assert!(contract
            .validate_changed_paths(["src/app/layout.tsx"])
            .is_empty());
        let violations = contract.validate_changed_paths(["src/app/layout.tsx", "src/auth.rs"]);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "execution-path-out-of-scope");
        assert_eq!(violations[0].path.as_deref(), Some("src/auth.rs"));
    }

    #[test]
    fn scoped_quick_edit_has_a_small_hard_change_budget() {
        let contract = ExecutionContract::from_route(
            &route(RouteClass::QuickEdit, Depth::Fast, &["src/seo/"]),
            "adjust SEO",
        );
        let violations = contract.validate_changed_paths([
            "src/seo/a.ts",
            "src/seo/b.ts",
            "src/seo/c.ts",
            "src/seo/d.ts",
            "src/seo/e.ts",
        ]);
        assert!(violations
            .iter()
            .any(|v| v.code == "execution-change-budget-exceeded"));
    }

    #[test]
    fn sensitive_surface_requires_an_explicit_claim_when_scope_is_unknown() {
        let contract = ExecutionContract::from_route(
            &route(RouteClass::QuickEdit, Depth::Fast, &[]),
            "change the heading",
        );
        assert!(contract.validate_changed_paths(["src/page.tsx"]).is_empty());
        let violations = contract.validate_changed_paths(["package.json"]);
        assert_eq!(violations[0].code, "execution-path-out-of-scope");
    }

    #[test]
    fn wildcard_and_directory_claims_are_supported_cross_platform() {
        let contract = ExecutionContract::from_route(
            &route(RouteClass::Debug, Depth::Fast, &["src/api/**", "tests/"]),
            "fix API",
        );
        assert!(contract
            .validate_changed_paths(["src\\api\\v1\\login.rs", "tests/login.rs"])
            .is_empty());
    }

    #[test]
    fn exact_relative_paths_may_contain_unicode_and_spaces() {
        let path = "docs/中文 文件.md";
        let contract = ExecutionContract::from_route(
            &route(RouteClass::QuickEdit, Depth::Fast, &[path]),
            r#"提交git记录 "docs/中文 文件.md""#,
        );
        assert_eq!(contract.allowed_paths, [path]);
        assert!(contract.validate_changed_paths([path]).is_empty());
        assert!(!contract.validate_changed_paths(["other.txt"]).is_empty());
    }

    #[test]
    fn one_exact_step_claim_cannot_inherit_the_whole_route_budget() {
        let contract = ExecutionContract::from_step(
            &route(RouteClass::Build, Depth::Deep, &[]),
            "change one file",
            &step("one", &["src/one.rs"]),
        );
        assert_eq!(contract.max_changed_files, 1);
    }

    #[test]
    fn missing_lightweight_scope_allows_bounded_source_discovery_only() {
        let contract = ExecutionContract::from_route(
            &route(RouteClass::QuickEdit, Depth::Fast, &[]),
            "change the visible heading",
        );
        assert!(contract
            .validate_changed_paths(["src/app/page.tsx"])
            .is_empty());
        assert!(!contract.validate_changed_paths([".git/config"]).is_empty());
        assert!(contract.prompt_block().contains("Discover the minimum"));
    }

    #[test]
    fn absolute_parent_and_repository_wide_claims_never_widen_the_contract() {
        let plan = Plan {
            steps: vec![step(
                "safe",
                &["src/safe.rs", "/etc/passwd", "../outside", "**"],
            )],
            risks: Vec::new(),
            open_questions: Vec::new(),
        };
        let contract = ExecutionContract::from_plan(
            &route(RouteClass::Build, Depth::Standard, &[]),
            "safe edit",
            &plan,
        );
        assert_eq!(contract.allowed_paths, ["src/safe.rs"]);
        assert!(!contract.validate_changed_paths(["src/other.rs"]).is_empty());
    }
}
