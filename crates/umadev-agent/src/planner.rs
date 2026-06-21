//! Dynamic phase planner — the "dynamic agent" layer.
//!
//! UmaDev's canonical pipeline is the full nine-phase chain
//! ([`umadev_spec::PHASE_CHAIN`]). Forcing EVERY task through all nine phases is
//! exactly the rigidity the SOTA agent literature warns against: a fixed
//! workflow is the right call for *well-defined* work, but a one-line bug fix
//! does not need similar-product research + three core documents + two human
//! confirmation gates + a delivery proof-pack. That rigidity is what makes a
//! pipeline feel "weak" on small or narrow tasks.
//!
//! This module classifies the requirement and tailors WHICH phases run, while
//! (1) preserving the canonical ORDER, and (2) keeping the confirm gates
//! whenever their guarded phase actually runs and the task is heavyweight
//! enough to warrant a human checkpoint.
//!
//! The classifier is deterministic (bilingual zh/en keyword + intent
//! heuristics) so it needs no model call and is fully unit-tested. A
//! brain-assisted refinement can layer on top later without changing this
//! contract. **Fail-open:** an unrecognised requirement falls back to the full
//! [`TaskKind::Greenfield`] pipeline — the planner never produces *fewer*
//! phases than the safe default by accident.

use umadev_spec::Phase;

/// The kind of work a requirement describes. Inferred deterministically by
/// [`classify`]; drives the tailored [`PhasePlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// A new product / app from scratch — the full pipeline.
    Greenfield,
    /// Frontend / UI only — skip the backend phase.
    FrontendOnly,
    /// Backend / API / data only — skip the frontend phase + its preview gate.
    BackendOnly,
    /// A small bug fix — skip research / docs / gates; go straight to a lean
    /// implement + quality pass.
    Bugfix,
    /// A refactor / cleanup — the same lean path as a bug fix.
    Refactor,
    /// Docs / research / planning only — no code phases.
    DocsOnly,
}

impl TaskKind {
    /// Stable identifier for logs and workflow state.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            TaskKind::Greenfield => "greenfield",
            TaskKind::FrontendOnly => "frontend_only",
            TaskKind::BackendOnly => "backend_only",
            TaskKind::Bugfix => "bugfix",
            TaskKind::Refactor => "refactor",
            TaskKind::DocsOnly => "docs_only",
        }
    }

    /// The ordered phases for this kind — always an order-preserving subset of
    /// [`umadev_spec::PHASE_CHAIN`]. A confirm gate is included only when the
    /// phase it guards runs AND the task is heavyweight enough to warrant a
    /// human checkpoint (the lean bug-fix / refactor paths skip the gates).
    #[must_use]
    pub fn phases(self) -> Vec<Phase> {
        use Phase::{
            Backend, Delivery, Docs, DocsConfirm, Frontend, PreviewConfirm, Quality, Research, Spec,
        };
        match self {
            TaskKind::Greenfield => vec![
                Research,
                Docs,
                DocsConfirm,
                Spec,
                Frontend,
                PreviewConfirm,
                Backend,
                Quality,
                Delivery,
            ],
            TaskKind::FrontendOnly => vec![
                Research,
                Docs,
                DocsConfirm,
                Spec,
                Frontend,
                PreviewConfirm,
                Quality,
                Delivery,
            ],
            TaskKind::BackendOnly => {
                vec![
                    Research,
                    Docs,
                    DocsConfirm,
                    Spec,
                    Backend,
                    Quality,
                    Delivery,
                ]
            }
            // Lean fast paths: no research/docs ceremony, no gates, no delivery
            // proof-pack — just plan the change, implement it, gate on quality.
            TaskKind::Bugfix | TaskKind::Refactor => vec![Spec, Frontend, Backend, Quality],
            TaskKind::DocsOnly => vec![Research, Docs, DocsConfirm],
        }
    }
}

/// A tailored, ordered plan of phases for a specific requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhasePlan {
    /// The inferred task kind.
    pub kind: TaskKind,
    /// Ordered phases to execute — a subset of [`umadev_spec::PHASE_CHAIN`].
    pub phases: Vec<Phase>,
    /// Human-readable reason, shown to the user for transparency.
    pub rationale: String,
}

impl PhasePlan {
    /// Whether `phase` is part of this plan.
    #[must_use]
    pub fn includes(&self, phase: Phase) -> bool {
        self.phases.contains(&phase)
    }

    /// Phases from the canonical chain that this plan skips.
    #[must_use]
    pub fn skipped(&self) -> Vec<Phase> {
        umadev_spec::PHASE_CHAIN
            .iter()
            .copied()
            .filter(|p| !self.phases.contains(p))
            .collect()
    }
}

/// Classify `requirement` and produce a tailored [`PhasePlan`]. Deterministic,
/// bilingual (zh / en), fail-open to [`TaskKind::Greenfield`].
#[must_use]
pub fn plan(requirement: &str) -> PhasePlan {
    let kind = classify(requirement);
    PhasePlan {
        kind,
        phases: kind.phases(),
        rationale: rationale_for(kind),
    }
}

/// Deterministic intent classification. Order matters: the narrowest intents
/// (bug fix, refactor, docs-only) are matched before the broad frontend /
/// backend split, which is matched before the greenfield default. Needles are
/// chosen to be distinctive (Chinese terms + multi-character English tokens) to
/// avoid substring false positives.
#[must_use]
pub fn classify(requirement: &str) -> TaskKind {
    let q = requirement.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));

    // 1. Bug fix — the narrowest, fastest path.
    if has(&[
        "修复",
        "修一下",
        "修个",
        "报错",
        "bug",
        "fixbug",
        "fix the",
        "fix a",
        "crash",
        "不工作",
        "失效",
        "坏了",
        "崩溃",
        "报错",
        "闪退",
        "hotfix",
    ]) {
        return TaskKind::Bugfix;
    }
    // 2. Refactor / cleanup.
    if has(&[
        "重构",
        "refactor",
        "整理代码",
        "优化代码",
        "clean up",
        "cleanup",
        "拆分模块",
        "tidy up",
        "代码结构",
    ]) {
        return TaskKind::Refactor;
    }
    // 3. Docs / research / planning only.
    if has(&[
        "写文档",
        "出文档",
        "只做调研",
        "research only",
        "只要文档",
        "写个方案",
        "写 prd",
        "写prd",
        "需求文档",
        "技术方案",
        "调研报告",
        "docs only",
    ]) {
        return TaskKind::DocsOnly;
    }
    // 4. Frontend vs backend split (distinctive tokens only).
    let frontend = has(&[
        "前端",
        "界面",
        "页面",
        "样式",
        "组件",
        "布局",
        "frontend",
        "tailwind",
        "react",
        "vue",
        "落地页",
    ]);
    let backend = has(&[
        "后端",
        "接口",
        "数据库",
        "服务端",
        "数据表",
        "鉴权",
        "backend",
        "graphql",
        "fastapi",
        "express",
        "微服务",
    ]);
    if frontend && !backend {
        return TaskKind::FrontendOnly;
    }
    if backend && !frontend {
        return TaskKind::BackendOnly;
    }

    // 5. Default — a full product build.
    TaskKind::Greenfield
}

/// The subset of `plan`'s skipped phases that are safe to skip TODAY within the
/// runner's gate-anchored three-block structure with zero downstream risk:
/// `Delivery` — the final phase, which runs AFTER the quality gate, so skipping
/// it (a lean bug-fix / refactor needs no deploy proof-pack) cannot affect any
/// gate or quality check. `Research` / `Backend` / `Frontend` and the lean
/// gate-skipping paths interact with later phases (the quality gate filters by
/// check name, not phase) and so are deferred to the full plan-driven runner
/// walk — the planner never claims a skip it does not actually perform.
#[must_use]
pub fn gate_safe_skips(plan: &PhasePlan) -> Vec<Phase> {
    plan.skipped()
        .into_iter()
        .filter(|p| matches!(p, Phase::Delivery))
        .collect()
}

/// One-line rationale per kind (localised at the call site is overkill here;
/// the runner surfaces this verbatim as a transparency note).
// Honest, advisory descriptions of how the task was classified. They describe
// the FOCUS, not a literal phase-skip — today the runner only auto-skips the
// Delivery phase (via gate_safe_skips); the rest of the pipeline still runs and
// pauses at its gates, so these must not promise skips that don't happen.
fn rationale_for(kind: TaskKind) -> String {
    match kind {
        TaskKind::Greenfield => "全新产品 — 走完整九阶段管线".to_string(),
        TaskKind::FrontendOnly => "偏前端 — 重点在前端实现与预览确认".to_string(),
        TaskKind::BackendOnly => "偏后端 — 重点在后端实现与前后端契约对齐".to_string(),
        TaskKind::Bugfix => "小修复 — 聚焦定位与最小改动,文档从简".to_string(),
        TaskKind::Refactor => "重构 — 聚焦结构调整、保持行为不变".to_string(),
        TaskKind::DocsOnly => "文档/调研为主 — 在文档确认门停下,由你决定是否继续实现".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_spec::Phase;

    #[test]
    fn classifies_bugfix() {
        assert_eq!(classify("修复登录页的 bug"), TaskKind::Bugfix);
        assert_eq!(classify("登录一直报错,帮我修一下"), TaskKind::Bugfix);
        assert_eq!(classify("the app crashes on submit"), TaskKind::Bugfix);
    }

    #[test]
    fn classifies_refactor() {
        assert_eq!(classify("重构 app.rs 拆分模块"), TaskKind::Refactor);
        assert_eq!(classify("refactor the auth module"), TaskKind::Refactor);
    }

    #[test]
    fn classifies_docs_only() {
        assert_eq!(classify("先写需求文档"), TaskKind::DocsOnly);
        assert_eq!(classify("写个方案给我看看"), TaskKind::DocsOnly);
    }

    #[test]
    fn classifies_frontend_and_backend() {
        assert_eq!(classify("做一个前端落地页"), TaskKind::FrontendOnly);
        assert_eq!(classify("build a React component"), TaskKind::FrontendOnly);
        assert_eq!(classify("写一个后端接口"), TaskKind::BackendOnly);
        assert_eq!(
            classify("a GraphQL backend with auth"),
            TaskKind::BackendOnly
        );
    }

    #[test]
    fn frontend_and_backend_together_is_greenfield() {
        // Mentions both sides → a full build, not a one-sided task.
        assert_eq!(
            classify("做一个带前端和后端的电商网站"),
            TaskKind::Greenfield
        );
    }

    #[test]
    fn defaults_to_greenfield() {
        assert_eq!(classify("做一个待办事项应用"), TaskKind::Greenfield);
        assert_eq!(classify("帮我做个 SaaS 产品"), TaskKind::Greenfield);
    }

    #[test]
    fn greenfield_runs_the_full_chain() {
        let p = plan("做一个电商平台");
        assert_eq!(p.kind, TaskKind::Greenfield);
        assert_eq!(p.phases, umadev_spec::PHASE_CHAIN.to_vec());
        assert!(p.skipped().is_empty());
    }

    #[test]
    fn bugfix_skips_research_docs_and_gates() {
        let p = plan("修复一个报错");
        assert_eq!(p.kind, TaskKind::Bugfix);
        assert!(!p.includes(Phase::Research));
        assert!(!p.includes(Phase::Docs));
        assert!(!p.includes(Phase::DocsConfirm));
        assert!(!p.includes(Phase::PreviewConfirm));
        assert!(!p.includes(Phase::Delivery));
        // …but still plans + quality-gates the change.
        assert!(p.includes(Phase::Spec));
        assert!(p.includes(Phase::Quality));
        let skipped = p.skipped();
        assert!(skipped.contains(&Phase::Research));
    }

    #[test]
    fn frontend_only_skips_backend_keeps_preview_gate() {
        let p = plan("做一个前端页面");
        assert!(p.includes(Phase::Frontend));
        assert!(p.includes(Phase::PreviewConfirm));
        assert!(!p.includes(Phase::Backend));
    }

    #[test]
    fn backend_only_skips_frontend_and_preview_gate() {
        let p = plan("写一个后端 graphql 接口");
        assert!(p.includes(Phase::Backend));
        assert!(!p.includes(Phase::Frontend));
        assert!(!p.includes(Phase::PreviewConfirm));
        // Docs gate still applies (it's a heavyweight build).
        assert!(p.includes(Phase::DocsConfirm));
    }

    #[test]
    fn gate_safe_skips_is_delivery_only_today() {
        // A bug fix plan skips many phases, but only Delivery is wired as a
        // zero-risk skip today (it runs after the quality gate).
        let p = plan("修复一个报错");
        assert_eq!(gate_safe_skips(&p), vec![Phase::Delivery]);
        // Greenfield skips nothing.
        assert!(gate_safe_skips(&plan("做一个电商网站")).is_empty());
    }

    #[test]
    fn every_plan_preserves_canonical_order() {
        for req in [
            "做一个电商网站",
            "做个前端页面",
            "写后端接口",
            "修复 bug",
            "重构代码",
            "写需求文档",
        ] {
            let p = plan(req);
            // The plan's phases appear in the same relative order as PHASE_CHAIN.
            let chain: Vec<Phase> = umadev_spec::PHASE_CHAIN.to_vec();
            let mut last = None;
            for ph in &p.phases {
                let idx = chain.iter().position(|c| c == ph).unwrap();
                if let Some(prev) = last {
                    assert!(idx > prev, "phase {ph:?} out of canonical order in {req}");
                }
                last = Some(idx);
            }
        }
    }
}
