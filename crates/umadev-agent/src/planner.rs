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
    /// A trivial change — a one-line tweak, a style nudge, a tiny script. The
    /// LIGHTEST path of all: a lean clarify-spec → implement → verify, with no
    /// research / docs / two confirm gates / delivery proof-pack. This is the
    /// answer to "the full nine phases are too heavy for a small task": the
    /// planner can auto-suggest it (see [`classify`]), and `umadev quick`
    /// forces it regardless of classification.
    Light,
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
            TaskKind::Light => "light",
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
            // The lightest path — for a trivial change the full nine phases are
            // pure overhead. A lean clarify-lite `Spec` → implement
            // (`Frontend` + `Backend`, whichever the change touches) → `Quality`
            // verify. No research, no three core docs, no two confirm gates, no
            // delivery proof-pack. Governance still applies on every write.
            TaskKind::Light => vec![Spec, Frontend, Backend, Quality],
        }
    }

    /// Whether this is the lightweight fast track (trivial work). The runner
    /// drives a [`TaskKind::Light`] plan through [`crate::AgentRunner::run_light`]
    /// in a single shot rather than the gate-anchored three-block walk.
    #[must_use]
    pub fn is_light(self) -> bool {
        matches!(self, TaskKind::Light)
    }

    /// Whether a director `/run` of this kind should take the **lean build tier**
    /// — a short firmware directive + a stripped-down QC pass — instead of the
    /// full commercial-grade framing + the duplicate build + the fork-review team.
    ///
    /// This is the single source of truth the director path consults so the
    /// directive ([`crate::experts::director_build_directive`]) and the auto-QC
    /// pass ([`crate::director_loop`]) stay in lockstep: BOTH go lean for exactly
    /// the same kinds, never one without the other.
    ///
    /// The lean tier is the kinds whose review teams are ALREADY empty
    /// ([`crate::critics::quality_team_for_kind`] returns `Vec::new()` for these):
    /// [`TaskKind::Light`] (a trivial / explicitly-small build), [`TaskKind::Bugfix`],
    /// and [`TaskKind::Refactor`]. Those are precisely the goals where a 12-minute
    /// "commercial-grade build + a second `npm install` + an 8-seat fork review"
    /// is pure overhead over a base that would just do it. Every heavyweight kind
    /// (Greenfield / FrontendOnly / BackendOnly / DocsOnly) keeps the full firmware
    /// + full QC — the research / docs / contract / review ARE the value there.
    ///
    /// Fail-open by construction: [`classify`] falls back to [`TaskKind::Greenfield`]
    /// on anything unrecognised, so an ambiguous requirement gets the FULL path —
    /// the lean tier is never reached by accident, only on a clearly-lean signal.
    #[must_use]
    pub fn is_lean_build(self) -> bool {
        matches!(
            self,
            TaskKind::Light | TaskKind::Bugfix | TaskKind::Refactor
        )
    }
}

/// Whether a director `/run` on `requirement` should take the **lean build tier**.
///
/// A thin convenience over [`classify`] + [`TaskKind::is_lean_build`] so the two
/// director call sites (the firmware directive and the auto-QC pass) classify a
/// requirement identically from one entry point. Deterministic, no model call,
/// fail-open to the FULL path (an unrecognised requirement classifies as
/// [`TaskKind::Greenfield`], whose [`TaskKind::is_lean_build`] is `false`).
#[must_use]
pub fn is_lean_build(requirement: &str) -> bool {
    classify(requirement).is_lean_build()
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
    let mut phases = kind.phases();
    // A simple (Light) build rarely needs BOTH a frontend and a backend. Trim the
    // surface the requirement never mentions so a pure static page doesn't pay for
    // an empty backend phase (~25% of a lean run was a do-nothing Backend turn),
    // and a small script doesn't pay for an empty frontend phase. Only trims when
    // the requirement is clearly one-sided; an ambiguous Light build keeps both.
    // Never touches a gated plan (Greenfield/FrontendOnly/BackendOnly already pick
    // their phases) — only the otherwise-fixed lean Light list.
    if kind == TaskKind::Light {
        let q = requirement.to_lowercase();
        let fe = mentions_frontend(&q);
        let be = mentions_backend(&q);
        if fe && !be {
            phases.retain(|p| *p != Phase::Backend);
        } else if be && !fe {
            phases.retain(|p| *p != Phase::Frontend);
        }
    }
    PhasePlan {
        kind,
        phases,
        rationale: rationale_for(kind),
    }
}

/// `true` when the requirement names a frontend / UI surface (used to trim a lean
/// plan's surplus phase). Distinctive tokens only — shared with [`classify`].
fn mentions_frontend(q: &str) -> bool {
    const FE: &[&str] = &[
        "前端",
        "界面",
        "页面",
        "单页",
        "样式",
        "组件",
        "布局",
        "静态页",
        "落地页",
        "frontend",
        "tailwind",
        "react",
        "vue",
        "html",
        "css",
        " ui",
        "single page",
        "static page",
        "landing page",
    ];
    FE.iter().any(|n| q.contains(n))
}

/// Whether `requirement` names a user-facing UI surface (a frontend page /
/// component / style). Public wrapper over the internal frontend-token check so the
/// router can decide whether a lean `Light` build actually ships UI (and thus
/// warrants the minimal UI review) or is a non-UI doc / script (which does not).
#[must_use]
pub fn mentions_ui_surface(requirement: &str) -> bool {
    mentions_frontend(&requirement.to_lowercase())
}

/// Whether `requirement` asks to produce a LIGHT documentation artifact — a README,
/// a changelog, a license, a contributing guide, a usage / install note, or a single
/// markdown doc. A doc FILE is a write-one-file task, NOT a product build, so it
/// takes the leanest path (no team, no pipeline, no review loop) — the user-reported
/// "generating a README runs a full review" case. This is deliberately DISTINCT from
/// [`TaskKind::DocsOnly`], which is a heavyweight PLANNING doc (a PRD / 需求文档 /
/// 技术方案 / 调研报告 that legitimately earns a PM + architect + designer doc-review):
/// a README is just a file. Callers still apply the internal heavy-signal veto so "a
/// readme-generator SaaS platform" (a real product that merely mentions a readme) is
/// never downgraded. Deterministic + fail-open.
#[must_use]
pub fn is_doc_task(requirement: &str) -> bool {
    is_doc_artifact(&requirement.to_lowercase())
}

/// Whether `requirement` asks to PRODUCE a document as the deliverable ITSELF — the
/// BROAD superset of [`is_doc_task`]: the README-class artifacts PLUS the planning /
/// spec / design-doc phrasings (a PRD / 需求文档 / 技术方案 / 设计文档 / design doc /
/// spec doc / 方案 / report / 报告 / 周报, "write / draft a … doc", …).
///
/// This is the **fail-open deterministic FLOOR** for the brain-first document sizing:
/// the router triage ([`crate::router`] `ROUTER_TRIAGE_SYSTEM`) is the AUTHORITATIVE
/// judge of "write a document vs. build the product" — this fires only when the brain
/// did not / could not decide (offline / unreachable). A document is a write-the-doc
/// task, NOT a product build, so without this floor a document whose phrasing missed
/// every narrower keyword would fall to the heavyweight [`TaskKind::Greenfield`] default
/// (a full 8-seat team building+reviewing a .md — the token-burn bug). Still vetoed by
/// the internal heavy-signal classifier so "build a docs PLATFORM" (a real product that merely mentions
/// documents) stays a product. Deterministic + fail-open.
///
/// The heavy-signal veto is baked IN here (unlike the raw [`is_doc_task`]) so
/// every caller — the QC short-circuit, the source-present floor, the TUI team
/// sizing — agrees that a docs-platform PRODUCT is NOT a document task without each
/// re-applying the veto.
#[must_use]
pub fn is_document_task(requirement: &str) -> bool {
    let q = requirement.to_lowercase();
    is_document_artifact(&q) && !has_heavy_signal(&q)
}

/// The document-task token check over an already-lowercased string — the shared body
/// of [`is_document_task`] and [`classify`]'s document step. A superset of
/// [`is_doc_artifact`]: the README-class artifacts PLUS planning / spec / design-doc
/// phrasings. Kept distinct from the README-only [`is_doc_artifact`] so the lighter
/// README path (→ [`TaskKind::Light`]) and the planning-doc path (→
/// [`TaskKind::DocsOnly`]) stay separable in [`classify`].
fn is_document_artifact(q: &str) -> bool {
    if is_doc_artifact(q) {
        return true;
    }
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    has(&[
        // Planning / spec / design / report docs (en) — the deliverable IS the document.
        // Doc-shaped phrasings only for the otherwise-ambiguous tokens ("spec" /
        // "方案" also read as "build TO this spec / 实现这个方案" — left to the brain),
        // so the offline floor never mis-sizes a build DOWN to a doc.
        "prd",
        "design doc",
        "design document",
        "spec doc",
        "tech spec",
        "technical spec",
        "specification",
        "requirements doc",
        "requirements document",
        "research report",
        "status report",
        "report doc",
        "whitepaper",
        "white paper",
        "proposal",
        "write a doc",
        "write the doc",
        "draft a doc",
        "draft the doc",
        "write a spec",
        "draft a spec",
        "write a report",
        "draft a report",
        // Planning / spec / design / report docs (zh).
        "需求文档",
        "需求说明",
        "技术方案",
        "设计文档",
        "设计方案",
        "方案文档",
        "调研报告",
        "技术文档",
        "产品文档",
        "规格说明",
        "规格文档",
        "报告",
        "周报",
        "日报",
        "月报",
        "写方案",
        "写个方案",
        "写一份方案",
        "写文档",
        "撰写文档",
    ])
}

/// The doc-artifact token check over an already-lowercased string — the shared body
/// of [`is_doc_task`] and [`classify`]'s doc step (so the latter reuses its own
/// lowercased `q` without re-allocating).
fn is_doc_artifact(q: &str) -> bool {
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    has(&[
        // Canonical repo doc files (en).
        "readme",
        "read me",
        "changelog",
        "change log",
        "license",
        "licence",
        "contributing",
        "code of conduct",
        "docstring",
        "doc string",
        // Canonical repo doc files / usage docs (zh).
        "更新日志",
        "变更日志",
        "更新记录",
        "许可证",
        "开源协议",
        "贡献指南",
        "行为准则",
        "使用说明",
        "使用文档",
        "使用手册",
        "说明文档",
        "安装说明",
        "安装文档",
        "操作手册",
        "用户手册",
        "用户指南",
        "用户文档",
        // A single markdown doc, explicitly.
        "markdown 文档",
        "markdown文档",
        "一个 markdown",
        "一个markdown",
        "一个 md 文件",
        "个 md 文件",
    ])
}

/// `true` when the requirement names a backend / server / data surface — shared
/// with [`classify`].
fn mentions_backend(q: &str) -> bool {
    const BE: &[&str] = &[
        "后端",
        "接口",
        "数据库",
        "服务端",
        "数据表",
        "鉴权",
        "脚本",
        "命令行",
        "backend",
        "graphql",
        "fastapi",
        "express",
        "微服务",
        "script",
        "cli",
        "api",
        "server",
    ];
    BE.iter().any(|n| q.contains(n))
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
    ]) && !(has_heavy_signal(&q) && asks_to_build_new_scope(&q))
    {
        // M1: a bug-fix verb that ALSO carries a heavyweight surface AND explicit
        // additive-build intent ("fix the checkout and build a full dashboard with
        // auth") is a real build wearing a fix verb — escalate it off the lean path
        // (which would skip research / docs / both confirm gates / delivery + run an
        // empty review team). The veto needs BOTH signals so a genuine bug-fix that
        // merely MENTIONS a heavy area ("修复登录页的小 bug") stays lean (no over-block).
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
    ]) && !(has_heavy_signal(&q) && asks_to_build_new_scope(&q))
    {
        // M1: same combined veto — "重构整个系统,加登录、支付、数据库" pairs a heavy
        // surface (登录/支付/数据库) with additive-build intent (整个系统/加登录), so it
        // is an additive heavyweight build, not a behavior-preserving refactor → escalate
        // it off the lean path. A pure refactor that only mentions a heavy area stays lean.
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
    // 4. Trivial change — the lightest of all. A one-line tweak, a tiny style
    //    nudge, a small script: the full nine phases are pure overhead. Needles
    //    are deliberately NARROW (explicit "small/tiny/trivial" markers + tiny
    //    artefacts) so a real feature never silently downgrades to Light; an
    //    ambiguous request still falls through to the heavyweight default.
    if has(&[
        "小改",
        "小修改",
        "微调",
        "改个文案",
        "改文案",
        "改个文字",
        "改个颜色",
        "改颜色",
        "改个样式",
        "改一行",
        "加个日志",
        "小脚本",
        "写个脚本",
        "small tweak",
        "tiny tweak",
        "minor tweak",
        "quick change",
        "trivial change",
        "one-liner",
        "one liner",
        "small script",
        "tiny script",
        "tweak the copy",
        "change the text",
        "rename ",
        "bump the version",
        "typo",
    ]) {
        return TaskKind::Light;
    }

    // 4.5. Light documentation artifact — a README / changelog / license /
    //      contributing guide / usage doc / a single markdown doc. A doc FILE is a
    //      write-one-file task, NOT a product build, so it takes the LEANEST path
    //      (Light): no team, no pipeline, no review loop. THIS is the root fix for the
    //      user-reported "generating a README runs a full review" case — without it a
    //      README matches no narrower needle and falls through to the heavyweight
    //      `Greenfield` default below. Guarded by `!has_heavy_signal` so a docs
    //      PLATFORM / a product that merely mentions a doc is never downgraded; a
    //      heavyweight PLANNING doc (PRD / 需求文档 / 技术方案) already matched DocsOnly
    //      above and keeps its full doc-review.
    if is_doc_artifact(&q) && !has_heavy_signal(&q) {
        return TaskKind::Light;
    }

    // 4.6. A planning / spec / design DOCUMENT as the deliverable itself — a PRD,
    //      技术方案, 设计文档, design doc, 调研报告 / report, 周报, … . This is the
    //      FAIL-OPEN deterministic floor for the brain-first document sizing: the
    //      router triage (`ROUTER_TRIAGE_SYSTEM`) is the AUTHORITATIVE judge of
    //      "write a document vs. build the product", and this only fires when the
    //      brain didn't / couldn't decide. A document is a write-the-doc task, NOT a
    //      product build, so WITHOUT this step it misses every narrower needle above
    //      and falls to the heavyweight `Greenfield` default below — the exact
    //      token-burn bug (a full team building+reviewing a .md). Routed to
    //      `DocsOnly`, the document kind, whose review scales to a single editorial
    //      PM read (`critics::docs_team_for_kind`). It runs BEFORE the frontend /
    //      backend split so "写一份前端设计文档" is a document, not FrontendOnly.
    //      Vetoed by `has_heavy_signal` so "build a docs PLATFORM" stays a real
    //      product. (README-class light artifacts already matched 4.5 → Light above.)
    if is_document_artifact(&q) && !has_heavy_signal(&q) {
        return TaskKind::DocsOnly;
    }

    // 5. Frontend vs backend split (distinctive tokens only).
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

    // 5.5. A genuinely SIMPLE small build — the lightweight path. "做一个简单的
    //      待办清单单页应用,纯前端" should NOT pay for research + three full core
    //      documents + two confirm gates; it should go spec → implement → verify.
    //      This is a SCOPED downgrade, guarded on BOTH sides:
    //      (a) it fires only when an explicit "this is small" signal is present
    //          (简单/单页/demo/小工具/静态页/single page/…), AND
    //      (b) it NEVER fires when a heavyweight-product signal is present
    //          (登录/auth/数据库/支付/SaaS/平台/多页/multi-module/…) — those keep
    //          their full pipeline because the research + docs + gates ARE the value.
    //      Fail-open: with no explicit-simple signal OR any heavy signal present,
    //      we fall through to the existing FrontendOnly / BackendOnly / Greenfield
    //      defaults, so a real product is never mis-downgraded.
    if is_simple_build(&q) && !has_heavy_signal(&q) {
        return TaskKind::Light;
    }

    if frontend && !backend {
        return TaskKind::FrontendOnly;
    }
    if backend && !frontend {
        return TaskKind::BackendOnly;
    }

    // 6. Default — a full product build.
    TaskKind::Greenfield
}

/// Whether `q` (already lowercased) carries an EXPLICIT "this is a small build"
/// signal — the positive half of the lightweight-build heuristic. These are
/// markers a user adds to say "keep it small": an explicit smallness word
/// (简单 / 小 / mini / demo / toy / quick), a single-page / static-page shape, or
/// a "tiny tool / small app" framing. A bare "做一个待办应用" carries NO such
/// signal, so it stays on the full pipeline (a real product) — only when the
/// user actually scoped it down do we consider the light path.
fn is_simple_build(q: &str) -> bool {
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    has(&[
        // Explicit smallness (zh).
        "简单的",
        "简易",
        "简单小",
        "极简",
        "单页应用",
        "单页面",
        "单个页面",
        "一个小",
        "小工具",
        "小demo",
        "小 demo",
        "小项目",
        "静态页",
        "静态网页",
        "纯静态",
        "练手",
        "小练习",
        "玩具项目",
        // Explicit smallness (en).
        "simple ",
        "single page",
        "single-page",
        "one page",
        "one-page",
        "static page",
        "static html",
        "tiny app",
        "small app",
        "little app",
        "mini app",
        "small tool",
        "tiny tool",
        "demo app",
        "toy app",
        "toy project",
        "just a simple",
        "quick demo",
        "quick prototype",
        "basic html",
    ])
}

/// Whether `q` (already lowercased) carries a HEAVYWEIGHT-product signal — the
/// negative half of the lightweight-build heuristic. Any of these means "this is
/// a real product even if phrased casually" (auth, persistence, payments,
/// multi-module / multi-page surface, an explicit commercial / production /
/// platform framing), so the light path is VETOED and the full pipeline stands.
/// This is the guardrail that keeps "做一个带邮箱登录的 SaaS 数据分析仪表盘" on
/// `Greenfield` no matter how the smallness words read.
fn has_heavy_signal(q: &str) -> bool {
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    has(&[
        // Persistence / accounts / payments / auth — anything that needs a backend
        // surface, a data model, or a security posture is NOT a light build.
        "登录",
        "注册",
        "账号",
        "账户",
        "鉴权",
        "权限",
        "数据库",
        "持久化",
        "后端",
        "服务端",
        "接口",
        "api",
        "支付",
        "订单",
        "结算",
        "上传",
        "实时",
        "推送",
        // E-commerce / marketplace — accounts + payments + inventory + persistence by
        // nature, so NEVER a light build no matter how casually phrased ("做一个简单的
        // 购物网站"). M6: the heavy list previously lacked commerce nouns, so a
        //商城 / 购物 / shop / store slipped onto the Light path (skipping research /
        // docs / both confirm gates / delivery for a product that needs all of them).
        "电商",
        "商城",
        "购物",
        "购物车",
        "网店",
        "店铺",
        "商店",
        "下单",
        "库存",
        "商品",
        "卖家",
        "买家",
        "auth",
        "login",
        "signup",
        "sign up",
        "sign-in",
        "oauth",
        "database",
        "backend",
        "server-side",
        "payment",
        "checkout",
        "stripe",
        // Commercial / scale / multi-surface framing — the research + docs + gates
        // are the product value here, so never downgrade.
        "商业级",
        "可上线",
        "上线",
        "生产级",
        "saas",
        "平台",
        "多页",
        "多模块",
        "后台管理",
        "管理系统",
        "仪表盘",
        "dashboard",
        "platform",
        "production",
        "multi-page",
        "multi page",
        "multi-module",
        "enterprise",
        "scalable",
        // E-commerce / marketplace (en).
        "ecommerce",
        "e-commerce",
        "shop",
        "store",
        "storefront",
        "marketplace",
        "shopping cart",
        "inventory",
    ])
}

/// Whether the requirement asks to BUILD / ADD genuinely NEW scope (a whole system,
/// a new feature/module, an extra surface) ON TOP OF a fix/refactor verb — the tell
/// that a "fix"/"refactor"-worded ask is actually an additive heavyweight build
/// ("重构整个系统,加登录、支付、数据库" / "fix the checkout and build a full dashboard
/// with auth"). Deliberately NARROW: it matches explicit build/add-a-thing phrasings
/// (a determiner/conjunction-anchored `build a|the|out`, `加X`, `搭建`, `新增…系统`),
/// NOT a bare heavy NOUN — so a genuine bug-fix that merely MENTIONS a heavy area
/// ("修复登录页的一个小 bug") is NOT mistaken for a build. Combined with
/// [`has_heavy_signal`] at the Bugfix/Refactor veto so escalation needs BOTH a heavy
/// surface AND additive-build intent.
fn asks_to_build_new_scope(q: &str) -> bool {
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    has(&[
        // English — an explicit "build/create a whole new thing" verb, anchored by a
        // determiner / conjunction so a bare "the build failed" never trips it.
        "build a",
        "build an",
        "build the ",
        "build out",
        "and build",
        "build a full",
        "full dashboard",
        "create a full",
        // Chinese — additive "build/add a system/feature/module" phrasings.
        "整个系统",
        "搭建",
        "新建",
        "加登录",
        "加支付",
        "加数据库",
        "加注册",
        "加鉴权",
        "新增功能",
        "新增模块",
        "新增系统",
    ])
}

/// Derive the governance [`ProjectContext`](umadev_governance::ProjectContext)
/// for a run — the signal that lets the rule engine skip server/security-surface
/// rules (CSP, clickjacking, structured logging, HSTS, HTTPS-redirect, CSRF,
/// token-context RNG) for a project that has no such surface.
///
/// This adds **no** model call. It reads three sources the run already has, and
/// is **conservative and fail-open**: it only returns the lenient
/// [`ProjectContext::static_frontend`](umadev_governance::ProjectContext::static_frontend)
/// when EVERY signal agrees the project is a static, frontend-only build with no
/// server/data/auth surface. ANY signal of a backend → the strict
/// [`ProjectContext::unknown`](umadev_governance::ProjectContext::unknown)
/// (assume a surface might exist), so a real backend/auth project is never
/// under-governed by accident.
///
/// Signals:
/// 1. **Task kind** — only [`TaskKind::FrontendOnly`] / [`TaskKind::Light`] are
///    candidates. [`TaskKind::Greenfield`] / [`TaskKind::BackendOnly`] and the
///    lean bug-fix/refactor paths always stay strict.
/// 2. **Requirement text** — any heavyweight signal (auth / database / payment /
///    api / backend / dashboard / platform — see the internal heavy-signal classifier) vetoes
///    the lenient context.
/// 3. **Architecture doc + produced source** — if the architecture doc declares
///    an API surface or data model, or any produced source file carries server
///    evidence (a listener, an API route, a backend framework import, auth/token
///    handling), the project HAS a surface → strict.
#[must_use]
pub fn derive_project_context(
    requirement: &str,
    project_root: &std::path::Path,
    slug: &str,
) -> umadev_governance::ProjectContext {
    derive_project_context_with_color(
        requirement,
        project_root,
        slug,
        stored_color_permission(project_root, requirement),
    )
}

/// The colour permission ALREADY RECORDED for `requirement` in this workspace, or `false`.
///
/// [`derive_project_context`] is called on every base tool call (`continuous::govern_tool_call`
/// re-derives so a static project that grows a server file re-arms strict), and it has no
/// brain and must never spawn one — a model call per write would be absurd. So it CARRIES
/// FORWARD the decision the run door already made ([`persist_project_context_with_color`]),
/// rather than re-deriving it.
///
/// Provenance-gated by [`umadev_governance::ProjectContext::if_current`]: the stored decision
/// is honoured only when it was derived from THIS requirement. A different requirement — or an
/// unstamped / unreadable / absent context — carries nothing forward and the rule stays ARMED.
/// So last quarter's violet rebrand cannot stand the band down for today's "no purple", and a
/// run whose door never consulted the brain simply keeps the default-reject.
pub(crate) fn stored_color_permission(project_root: &std::path::Path, requirement: &str) -> bool {
    let Ok(raw) = crate::bounded_fs::read_utf8_beneath(
        project_root,
        &project_root.join(GOVERNANCE_CONTEXT_REL),
        MAX_PLANNER_METADATA_BYTES,
    ) else {
        return false;
    };
    let key = umadev_state::privacy::installation_key();
    serde_json::from_str::<umadev_governance::ProjectContext>(&raw)
        .map(|ctx| {
            ctx.if_current(
                now_secs(),
                Some(requirement),
                key.as_ref().map(<[u8; 32]>::as_slice),
            )
            .purple_allowed
        })
        .unwrap_or(false)
}

/// [`derive_project_context`] with the colour permission supplied by the caller — the form the
/// RUN DOOR uses, once, right after it has asked the brain
/// ([`crate::color_permission::consult_color_permission`]).
///
/// `purple_allowed` is the ONE stand-down of the banned-hue default-reject, and it is an
/// INTENT judgement, so it is not derived here: this crate owns no model, and a word list
/// cannot answer "did the user authorize this hue?" (it was tried, and it leaked on every
/// review round). The decision rides on EVERY return path below because it is orthogonal to
/// the server-surface question, and it must reach the WRITE governor
/// (`scan_content_with_context` → the PreToolUse hook), not just the design floor — otherwise
/// the user who chose the palette cannot write it and has nothing to fix.
#[must_use]
pub fn derive_project_context_with_color(
    requirement: &str,
    project_root: &std::path::Path,
    slug: &str,
    purple_allowed: bool,
) -> umadev_governance::ProjectContext {
    let purple = purple_allowed;
    // PROVENANCE, on every return path. The context is persisted and read back by other
    // PROCESSES (the PreToolUse hook, `umadev ci` in the pre-commit hook) that have no idea
    // what produced it — so it carries WHICH requirement it was derived from and WHEN.
    // Without that stamp a permission has no expiry and no owner: a `purple_allowed: true`
    // from an old violet rebrand would stand the banned-hue band down for every later
    // requirement, including one whose first line is "no purple". See
    // `ProjectContext::if_current`, which is what the readers apply.
    let provenance_key = umadev_state::privacy::installation_key();
    let stamp = |ctx: umadev_governance::ProjectContext| {
        provenance_key
            .as_ref()
            .map_or(ctx, |key| ctx.derived_from(requirement, key, now_secs()))
    };
    let strict = stamp(umadev_governance::ProjectContext::unknown().with_purple_allowed(purple));

    // Signal 1: task kind must be a frontend-only / light build.
    let kind = classify(requirement);
    if !matches!(kind, TaskKind::FrontendOnly | TaskKind::Light) {
        return strict;
    }

    // Signal 2: no heavyweight (auth/db/payment/api/backend/…) requirement words.
    if has_heavy_signal(&requirement.to_lowercase()) {
        return strict;
    }

    // Signal 3a: the architecture doc must NOT declare a server/data surface.
    // Read it best-effort; absent/empty doc is fine for a light build (it often
    // has no architecture doc at all), so absence is NOT a veto here — the other
    // signals already established "frontend-only, no heavy words".
    let arch_path = project_root.join(format!("output/{slug}-architecture.md"));
    let arch =
        match crate::bounded_fs::read_utf8_beneath(project_root, &arch_path, MAX_PLANNER_DOC_BYTES)
        {
            Ok(content) => content.to_lowercase(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            // An existing architecture contract that cannot be read in full must
            // not be mistaken for evidence that the project is static-only.
            Err(_) => return strict,
        };
    if doc_declares_server_surface(&arch) {
        return strict;
    }

    // Signal 3b: no produced source file may carry server evidence. If even one
    // does, the project grew a backend → strict. (Reuses the governance kernel's
    // per-file server-evidence detector by scanning each file through it.)
    if any_source_has_server_surface(project_root) {
        return strict;
    }

    stamp(umadev_governance::ProjectContext::static_frontend().with_purple_allowed(purple))
}

/// UNIX seconds, or 0 when the clock is unreadable (which reads as "unstamped" — the
/// strict direction).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Workspace-relative path of the persisted governance context.
pub const GOVERNANCE_CONTEXT_REL: &str = ".umadev/governance-context.json";
const MAX_PLANNER_METADATA_BYTES: usize = 1024 * 1024;
const MAX_PLANNER_DOC_BYTES: usize = 2 * 1024 * 1024;
const MAX_PLANNER_SOURCE_TOTAL_BYTES: usize = 32 * 1024 * 1024;

/// **Derive the run's governance [`ProjectContext`](umadev_governance::ProjectContext) and
/// PERSIST it** — the single place any run path writes the rule book that the other
/// surfaces read back.
///
/// The context is not an in-process detail. It is the answer to "did the user ask for
/// this?", and three surfaces need that answer:
///
/// - the in-process write scan (this run),
/// - the out-of-process PreToolUse hook (`umadev hook`, a separate process per tool call),
/// - and `umadev ci` — which is the one that actually FAILS a commit, out of
///   `.git/hooks/pre-commit`.
///
/// A run that honours "make our brand violet" in memory but never writes the context leaves
/// `ci` to judge with [`ProjectContext::unknown`](umadev_governance::ProjectContext::unknown)
/// — so the pre-commit hook blocks UD-CODE-002 on the exact color the user asked for, exit
/// 1, and there is nothing the user can edit to converge: the run accepts what the gate
/// refuses. Every path that can write code must therefore pass through here FIRST, before
/// the first file is written.
///
/// The COLOUR permission is not re-derived here — it is carried forward from whatever the
/// run door already recorded for this same requirement (see the internal stored-color resolver).
/// This function runs on every base tool call and has no brain; only
/// [`persist_project_context_with_color`] may set the permission.
///
/// Fail-open: an unwritable `.umadev/` is swallowed (the readers then default to full
/// strictness — conservative, never a false "clean"), and the derived context is returned
/// regardless so the in-process caller is never blocked by a persistence failure.
pub fn persist_project_context(
    requirement: &str,
    project_root: &std::path::Path,
    slug: &str,
) -> umadev_governance::ProjectContext {
    let ctx = derive_project_context(requirement, project_root, slug);
    write_project_context(project_root, &ctx);
    ctx
}

/// [`persist_project_context`] with the brain's colour verdict — the RUN-DOOR form, called
/// exactly ONCE per run, the instant the requirement is known and before the first file is
/// written.
///
/// This is the only writer of [`umadev_governance::ProjectContext::purple_allowed`]. Every
/// other surface (the PreToolUse hook, `umadev ci`, the design floor, and
/// [`persist_project_context`]'s per-tool-call refresh) READS the decision this call stored.
/// One decision, one writer, many readers — a gate that judges by a different rule book than
/// the run is unconvergeable by construction, and a per-write model call is not an option.
///
/// Fail-open: an unwritable `.umadev/` is swallowed and the context is returned regardless.
pub fn persist_project_context_with_color(
    requirement: &str,
    project_root: &std::path::Path,
    slug: &str,
    purple_allowed: bool,
) -> umadev_governance::ProjectContext {
    let ctx = derive_project_context_with_color(requirement, project_root, slug, purple_allowed);
    write_project_context(project_root, &ctx);
    ctx
}

/// Best-effort write of the context to [`GOVERNANCE_CONTEXT_REL`]. Fail-open: an unwritable
/// `.umadev/` is swallowed, and the readers then default to full strictness.
fn write_project_context(project_root: &std::path::Path, ctx: &umadev_governance::ProjectContext) {
    let Ok(dir) =
        crate::bounded_fs::ensure_real_dir_beneath(project_root, std::path::Path::new(".umadev"))
    else {
        return;
    };
    if let Ok(json) = serde_json::to_vec_pretty(ctx) {
        if json.len() <= MAX_PLANNER_METADATA_BYTES {
            let _ = umadev_state::fs::atomic_write(&dir.join("governance-context.json"), &json);
        }
    }
}

/// Whether an architecture doc (already lowercased) declares a real server /
/// data / auth surface — an API section/table, a data model, an auth scheme, or
/// a backend framework. Conservative: any such marker means "has a surface".
fn doc_declares_server_surface(arch_lower: &str) -> bool {
    if arch_lower.trim().is_empty() {
        return false;
    }
    [
        "## api",
        "api surface",
        "## data model",
        "数据模型",
        "数据库",
        "database",
        "endpoint",
        "/api/",
        "auth",
        "鉴权",
        "session",
        "会话",
        "jwt",
        "token",
        "backend",
        "服务端",
        "后端",
    ]
    .iter()
    .any(|needle| arch_lower.contains(needle))
}

/// Whether ANY produced source file carries its own server / security surface,
/// per the governance kernel's per-file detector. Scans through the public
/// `scan_content`-adjacent path: a file is a server surface iff governing it as
/// a static frontend would NOT skip the surface rules. Bounded + fail-open.
fn any_source_has_server_surface(project_root: &std::path::Path) -> bool {
    // A file that the static-frontend context would STILL govern at full
    // strictness is, by definition, one that carries server evidence. We probe
    // that by checking a server-surface rule under the lenient context: if it
    // would fire, the file has a surface. Cheaper + equivalent: rely on the
    // detector indirectly by scanning a benign HTML-shaped probe per file is not
    // possible from here, so we read each file and look for the same evidence the
    // kernel uses, via the public lenient/strict differential.
    let files = crate::acceptance::source_files(project_root);
    let mut budget = crate::bounded_fs::Utf8ReadBudget::new(
        MAX_PLANNER_SOURCE_TOTAL_BYTES,
        MAX_PLANNER_DOC_BYTES,
    );
    for f in files.iter().take(400) {
        let Ok(content) = budget.read_utf8_beneath(project_root, f) else {
            // This predicate controls whether strict server-surface governance
            // is armed. Unavailable evidence must choose the strict direction.
            return true;
        };
        let rel = f
            .strip_prefix(project_root)
            .unwrap_or(f)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if umadev_governance::file_has_server_surface(&rel, &content) {
            return true;
        }
    }
    false
}

/// Parse a user-supplied phase name into a typed [`Phase`], for `umadev redo
/// <phase>` / `/redo <phase>`. Case-insensitive and whitespace-tolerant, and
/// accepts the common friendly aliases a user is likely to type (`fe`/`ui` for
/// frontend, `be`/`api` for backend, `qa` for quality, etc.) in addition to the
/// canonical [`Phase::id`] strings. Returns `None` for anything unrecognised so
/// the caller can show the valid set — fail-open, never panics.
#[must_use]
pub fn phase_from_id(name: &str) -> Option<Phase> {
    match name
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "research" => Some(Phase::Research),
        "docs" | "doc" | "documents" => Some(Phase::Docs),
        "docs_confirm" | "docsconfirm" => Some(Phase::DocsConfirm),
        "spec" | "plan" => Some(Phase::Spec),
        "frontend" | "fe" | "ui" | "front" => Some(Phase::Frontend),
        "preview_confirm" | "previewconfirm" | "preview" => Some(Phase::PreviewConfirm),
        "backend" | "be" | "api" | "back" => Some(Phase::Backend),
        "quality" | "qa" | "quality_gate" => Some(Phase::Quality),
        "delivery" | "deliver" | "release" => Some(Phase::Delivery),
        _ => None,
    }
}

/// The phase names a user can pass to `redo`, in canonical chain order — used
/// to build a friendly "valid phases: …" error when [`phase_from_id`] rejects.
#[must_use]
pub fn redoable_phase_ids() -> Vec<&'static str> {
    umadev_spec::PHASE_CHAIN.iter().map(|p| p.id()).collect()
}

/// An **advisory prior** the director may read — Wave 3 of
/// `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` §3 (planner DEMOTED from "decides the
/// fixed phase list" to "an advisory prior").
///
/// In the Wave 3 model the director (thinking through the base) decides on the
/// spot how to get a goal done — who to bring in, in what order, how much process.
/// The planner no longer DICTATES that route; it only offers a *suggestion* the
/// director can take or ignore. This function renders the classifier's read of a
/// requirement as a SHORT, explicitly-advisory hint: "this looks like X; a goal
/// like this usually benefits from Y — but YOU judge what THIS one needs." It
/// deliberately uses non-binding language and never names a forced phase chain.
///
/// Deterministic + fail-open: it reuses [`classify`] (no model call), so an
/// unrecognised requirement falls back to the [`TaskKind::Greenfield`] advisory —
/// the most thorough suggestion, which the director is still free to trim.
#[must_use]
pub fn advisory_prior(requirement: &str) -> String {
    let kind = classify(requirement);
    // The non-binding "you might consider" framing per inferred kind. Each line
    // describes the SHAPE of work a goal like this usually warrants, then hands the
    // call back to the director — it is a prior, not an order.
    let hint = match kind {
        TaskKind::Greenfield => {
            "This reads like a full product build from scratch. A goal this size \
             usually benefits from framing the requirements (PM) and an approach \
             (architect) before splitting frontend / backend, then a QA + security \
             pass — but scale that to what THIS goal actually needs."
        }
        TaskKind::FrontendOnly => {
            "This reads like frontend / UI work. It usually centres on the design \
             system + the components, with a quick contract check that the calls \
             line up — little or no backend. Bring in a designer / frontend seat as \
             you see fit."
        }
        TaskKind::BackendOnly => {
            "This reads like backend / API / data work. It usually centres on the \
             routes + data model + validation, with the frontend contract kept in \
             mind — little or no UI. Bring in an architect / backend seat as you \
             see fit."
        }
        TaskKind::Bugfix => {
            "This reads like a focused bug fix. It usually wants a minimal, targeted \
             change and a check that the original failure path is actually gone — no \
             research / docs ceremony. Often one engineer end to end."
        }
        TaskKind::Refactor => {
            "This reads like a refactor / cleanup. It usually wants a structural \
             change that keeps behaviour identical — lean on the existing tests to \
             prove nothing broke. No new docs ceremony."
        }
        TaskKind::DocsOnly => {
            "This reads like docs / research / planning, not code. It usually wants \
             the writing done well and a checkpoint with the user before any \
             implementation."
        }
        TaskKind::Light => {
            "This reads like a small, scoped change. It usually wants you to just do \
             it directly — implement + a quick verify — with none of the research / \
             docs / gate ceremony a full product needs."
        }
    };
    format!(
        "[advisory — a prior you may use or ignore] {hint} The plan is YOURS to \
         decide; this is only a read of the goal, not a route you must follow."
    )
}

/// Build a plan that FORCES the lightweight fast track regardless of how the
/// requirement classifies. This is what `umadev quick` / `/quick` use: the user
/// has explicitly asked for the lean path, so we skip classification and pin
/// [`TaskKind::Light`]. The deterministic classifier still drives the default
/// `umadev run` path, where a trivial requirement is auto-suggested into Light
/// but the user can override by running the full pipeline instead.
#[must_use]
pub fn plan_light(requirement: &str) -> PhasePlan {
    let _ = requirement; // reserved for future per-requirement light tailoring
    let kind = TaskKind::Light;
    PhasePlan {
        kind,
        phases: kind.phases(),
        rationale: rationale_for(kind),
    }
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
        TaskKind::Light => {
            "轻量档 — 极简流程:澄清简版→实现→验证,跳过调研/三文档/两道确认门/交付物料包".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_spec::Phase;

    #[test]
    fn lean_build_tier_matches_the_empty_review_kinds() {
        // The lean build tier must be EXACTLY the kinds whose quality review team
        // is already empty (`quality_team_for_kind` returns Vec::new()): Light /
        // Bugfix / Refactor. Those are the goals where the heavy firmware + the
        // duplicate build + the fork review are pure overhead. Every heavyweight
        // kind keeps the FULL path.
        assert!(TaskKind::Light.is_lean_build());
        assert!(TaskKind::Bugfix.is_lean_build());
        assert!(TaskKind::Refactor.is_lean_build());
        assert!(!TaskKind::Greenfield.is_lean_build());
        assert!(!TaskKind::FrontendOnly.is_lean_build());
        assert!(!TaskKind::BackendOnly.is_lean_build());
        assert!(!TaskKind::DocsOnly.is_lean_build());
        // Cross-check one direction so the two never drift: EVERY lean-build kind
        // has an already-empty quality review team (so the lean QC short-circuit
        // can only ever skip a team that would have returned "no blocking" anyway —
        // it changes wall-clock, never the verdict). The converse does NOT hold:
        // `DocsOnly` also has an empty quality team (it produces no code to review)
        // yet is NOT a lean *build* tier — it keeps the full docs firmware/process.
        for k in [TaskKind::Light, TaskKind::Bugfix, TaskKind::Refactor] {
            assert!(
                crate::critics::quality_team_for_kind(k).is_empty(),
                "a lean-build kind must already have an empty quality team: {k:?}"
            );
        }
        // The heavyweight BUILD kinds (those that produce code) keep a non-empty
        // quality team — the lean tier must never swallow one of these.
        for k in [TaskKind::Greenfield, TaskKind::BackendOnly] {
            assert!(
                !k.is_lean_build() && !crate::critics::quality_team_for_kind(k).is_empty(),
                "a heavyweight build kind keeps its review team: {k:?}"
            );
        }
    }

    #[test]
    fn is_lean_build_requirement_classifies_and_fails_open_heavy() {
        // An explicitly-small build is lean; a real product (and an unrecognised
        // requirement, which falls back to Greenfield) is NOT lean — so a real
        // product never reaches the lean fast path by accident.
        assert!(is_lean_build("帮我改个文案")); // Light
        assert!(is_lean_build("修复登录按钮点击没反应")); // Bugfix
        assert!(is_lean_build("重构 app.rs 拆分模块")); // Refactor
        assert!(is_lean_build("做一个简单的待办清单单页应用,纯前端")); // Light
                                                                       // Heavyweight stays heavyweight (full firmware + full QC).
        assert!(!is_lean_build("做一个记账应用")); // Greenfield (no smallness signal)
        assert!(!is_lean_build("做一个带邮箱登录的 SaaS 数据分析仪表盘")); // Greenfield
        assert!(!is_lean_build("做一个前端落地页")); // FrontendOnly
        assert!(!is_lean_build("写一个后端接口")); // BackendOnly
                                                   // Unrecognised / empty → Greenfield → NOT lean (fail-open to the full path).
        assert!(!is_lean_build(""));
        assert!(!is_lean_build("?!@#"));
    }

    #[test]
    fn classifies_bugfix() {
        // Pure bug-fix asks with NO heavyweight signal stay on the lean Bugfix path.
        assert_eq!(classify("修复首页排版的 bug"), TaskKind::Bugfix);
        assert_eq!(classify("这个功能一直报错,帮我修一下"), TaskKind::Bugfix);
        assert_eq!(classify("the app crashes on submit"), TaskKind::Bugfix);
    }

    #[test]
    fn classifies_refactor() {
        // Behavior-preserving refactors with NO heavyweight signal stay lean.
        assert_eq!(classify("重构 app.rs 拆分模块"), TaskKind::Refactor);
        assert_eq!(classify("refactor the parser module"), TaskKind::Refactor);
    }

    #[test]
    fn heavy_bugfix_or_refactor_escalates_off_the_lean_path() {
        // M1 regression: a bug-fix / refactor VERB carrying a heavyweight signal
        // (auth / payment / database / dashboard / …) is an additive product build,
        // not a lean fix. The `has_heavy_signal` veto must keep it OFF the lean path
        // (which would skip research / docs / both confirm gates / delivery + run an
        // empty review team). It escalates to a heavyweight kind.
        assert!(
            !classify("重构整个系统,加登录、支付、数据库").is_lean_build(),
            "a refactor that ADDS auth/payment/db is a heavyweight build, not a lean refactor"
        );
        assert!(
            !classify("fix the checkout and build a full dashboard with auth").is_lean_build(),
            "a 'fix' that also builds a full dashboard with auth must escalate, not stay lean"
        );
        // Sanity: the lean verbs without any heavy signal still classify lean.
        assert!(classify("修复首页的小 bug").is_lean_build());
        assert!(classify("refactor the parser module").is_lean_build());
    }

    #[test]
    fn classifies_docs_only() {
        assert_eq!(classify("先写需求文档"), TaskKind::DocsOnly);
        assert_eq!(classify("写个方案给我看看"), TaskKind::DocsOnly);
    }

    #[test]
    fn classifies_doc_artifacts_as_light() {
        // The user-reported case: generating a README / changelog / license / usage
        // doc is a write-one-file task — the LEANEST path (Light), NOT a Greenfield
        // product build. Before this fix a README matched no narrower needle and fell
        // through to the heavyweight `Greenfield` default → a full review team.
        for r in [
            "生成一个 README.md",
            "帮我写个 README",
            "generate a README.md for this project",
            "write a readme",
            "生成更新日志",
            "update the changelog", // "update" + "changelog" → doc artifact (Light)
            "加一个 LICENSE 文件",
            "写一份使用说明文档",
            "生成安装说明",
            "create a CONTRIBUTING guide",
            "写一个 markdown 文档介绍用法",
        ] {
            assert_eq!(
                classify(r),
                TaskKind::Light,
                "doc artifact should be Light: {r}"
            );
            // A Light doc is on the lean tier → the QC short-circuit fires and the
            // heavy firmware framing is skipped (the single gate the cost reduction
            // keys off — see `director_loop::run_auto_qc` / `experts`).
            assert!(is_lean_build(r), "a doc artifact is a lean build: {r}");
        }
    }

    #[test]
    fn doc_artifact_does_not_downgrade_a_product_or_planning_doc() {
        // A real product that merely MENTIONS a readme (a readme-generator platform)
        // is vetoed by the heavy signal → never the lean Light path.
        assert_ne!(
            classify("做一个 readme 生成器 SaaS 平台,带账号和数据库"),
            TaskKind::Light,
        );
        // A PLANNING doc (PRD / 需求文档 / 技术方案 / 设计文档 / report) stays a
        // DocsOnly DOCUMENT task — NOT downgraded to a Light README, and NOT a product.
        assert_eq!(classify("先写需求文档"), TaskKind::DocsOnly);
        assert_eq!(classify("写个技术方案"), TaskKind::DocsOnly);
        // The token-burn fix: a planning document now convenes a SINGLE editorial PM
        // seat (no longer the old PM + architect + designer trio), and ZERO code-review
        // team — a document is not a product build.
        let docs = crate::critics::docs_team_for_kind(TaskKind::DocsOnly);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].role(), "product-manager");
        assert!(crate::critics::quality_team_for_kind(TaskKind::DocsOnly).is_empty());
        // And the lean README path convenes NO team at any stage.
        assert!(crate::critics::Seat::team_for_kind(TaskKind::Light).is_empty());
        assert!(crate::critics::quality_team_for_kind(TaskKind::Light).is_empty());
        assert!(crate::critics::docs_team_for_kind(TaskKind::Light).is_empty());
    }

    #[test]
    fn doc_and_ui_surface_helpers() {
        // `is_doc_task` recognises doc artifacts, not products.
        assert!(is_doc_task("生成一个 README.md"));
        assert!(is_doc_task("write a CHANGELOG"));
        assert!(!is_doc_task("做一个待办应用"));
        assert!(!is_doc_task("修复登录 bug"));
        // `mentions_ui_surface` recognises a frontend surface (so a Light UI page keeps
        // its review) but not a doc / script.
        assert!(mentions_ui_surface("做一个简单的待办单页应用,纯前端"));
        assert!(mentions_ui_surface("a small landing page"));
        assert!(!mentions_ui_surface("生成一个 README.md"));
        assert!(!mentions_ui_surface("写个脚本统计行数"));
    }

    #[test]
    fn document_tasks_are_docs_only_not_greenfield() {
        // The token-burn fix (deterministic fail-open floor): a request to WRITE a
        // planning / spec / design / report DOCUMENT — phrased many ways the narrower
        // keyword tables don't list — is a DOCUMENT task (DocsOnly, the lean document
        // kind), NOT the heavyweight Greenfield product build a missed keyword used to
        // fall through to (a full 8-seat team building+reviewing a .md).
        for r in [
            "帮我写一份产品需求文档(PRD)",
            "写一个技术方案",
            "produce a design doc for the onboarding flow",
            "draft a technical spec document",
            "写一份系统设计文档",
            "帮我出一个调研报告",
            "写本周的周报",
            "write a research report on caching strategies",
            "撰写一份产品文档",
        ] {
            assert_eq!(classify(r), TaskKind::DocsOnly, "should be DocsOnly: {r}");
            assert!(is_document_task(r), "should be a document task: {r}");
            // A document is NOT Greenfield, and convenes at most a single editorial PM
            // seat (no code-review team at all).
            assert_ne!(classify(r), TaskKind::Greenfield, "never Greenfield: {r}");
            assert_eq!(
                crate::critics::docs_team_for_kind(classify(r)).len(),
                1,
                "a document convenes one editorial PM seat: {r}"
            );
            assert!(
                crate::critics::quality_team_for_kind(classify(r)).is_empty(),
                "a document convenes no code-review team: {r}"
            );
        }
    }

    #[test]
    fn document_task_veto_keeps_a_product_and_a_real_build_unchanged() {
        // A docs PLATFORM / product that merely mentions documents is VETOED by the
        // heavy signal → never the lean document kind, it stays a real product build.
        assert_ne!(classify("做一个 PRD 管理 SaaS 平台"), TaskKind::DocsOnly);
        assert!(!is_document_task("做一个 prd 管理 saas 平台"));
        // A real product build is UNCHANGED — still Greenfield (full roster + gates).
        assert_eq!(classify("做一个电商平台"), TaskKind::Greenfield);
        assert_eq!(classify("做一个待办事项应用"), TaskKind::Greenfield);
        // is_document_task is false for product builds / code work / a README artifact
        // (a README is a Light file-write, handled separately — not a planning doc).
        assert!(!is_document_task("做一个电商平台"));
        assert!(!is_document_task("修复登录 bug"));
        assert!(!is_document_task("做一个简单的待办单页应用,纯前端"));
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
    fn an_explicitly_simple_ecommerce_build_is_not_downgraded_to_light() {
        // MEDIUM M6: a commerce build needs accounts + payments + inventory +
        // persistence, so even when phrased as "simple" it must NOT take the Light path
        // (which skips research / docs / both confirm gates / delivery). The heavy
        // signal now carries commerce nouns, so the smallness words are vetoed.
        for r in [
            "做一个简单的电商网站",
            "做一个简易的购物商城",
            "做一个简单的网上商店单页",
            "a simple ecommerce site",
            "build a simple online store",
            "just a simple shopping cart app",
            "a small marketplace demo",
        ] {
            assert_ne!(
                classify(r),
                TaskKind::Light,
                "a commerce build must not downgrade to Light: {r}"
            );
            assert!(
                !is_lean_build(r),
                "a commerce build is not a lean fast-path build: {r}"
            );
        }
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
    fn classifies_trivial_as_light() {
        assert_eq!(classify("帮我改个文案"), TaskKind::Light);
        assert_eq!(classify("这里微调一下间距"), TaskKind::Light);
        assert_eq!(classify("写个脚本批量重命名文件"), TaskKind::Light);
        assert_eq!(
            classify("a small tweak to the header copy"),
            TaskKind::Light
        );
        assert_eq!(classify("just a typo in the readme"), TaskKind::Light);
        // Ordering note: a request phrased as a "fix" matches the narrower
        // Bugfix lean path FIRST — both are lean, so this is fine.
        assert_eq!(classify("fix a typo in the readme"), TaskKind::Bugfix);
    }

    #[test]
    fn non_trivial_does_not_downgrade_to_light() {
        // A real feature / product must NOT silently become Light.
        assert_eq!(classify("做一个待办事项应用"), TaskKind::Greenfield);
        assert_eq!(classify("做一个前端落地页"), TaskKind::FrontendOnly);
        assert_eq!(classify("写一个后端接口"), TaskKind::BackendOnly);
    }

    #[test]
    fn simple_single_page_build_is_light() {
        // The dogfood case: an explicitly-simple, single-page, pure-frontend build
        // must take the lightweight path (spec -> implement -> verify), NOT the
        // full research + three-docs + gate pipeline that took 24 minutes.
        assert_eq!(
            classify("做一个简单的待办清单单页应用,纯前端 HTML+CSS+JS,支持添加删除"),
            TaskKind::Light
        );
        assert_eq!(classify("做一个简单的计算器单页面"), TaskKind::Light);
        assert_eq!(classify("写一个静态页展示个人简介"), TaskKind::Light);
        assert_eq!(
            classify("a simple single-page todo app, pure HTML/CSS/JS"),
            TaskKind::Light
        );
        assert_eq!(
            classify("build a tiny demo app, just a static page"),
            TaskKind::Light
        );
        assert_eq!(classify("做一个小工具帮我格式化 JSON"), TaskKind::Light);
    }

    #[test]
    fn heavy_signal_vetoes_the_light_downgrade() {
        // The boundary the planner must NOT cross: a smallness word does NOT make a
        // real product lean. Any auth / database / payment / SaaS / platform / dashboard
        // signal keeps the FULL pipeline — research + three docs + gates are its value.
        assert_eq!(
            classify("做一个带邮箱登录的 SaaS 数据分析仪表盘,要能上线"),
            TaskKind::Greenfield
        );
        // "简单的" present but a database + login veto the light path → it routes to
        // a heavyweight bucket (never the lean Light path).
        assert_ne!(
            classify("做一个简单的博客平台,带登录和数据库"),
            TaskKind::Light
        );
        // "single page" present but it's a real dashboard with auth + a backend api
        // → stays heavyweight (never the lean Light path).
        assert_ne!(
            classify("a single page dashboard with user login and a backend api"),
            TaskKind::Light
        );
        // A simple-sounding but payment-bearing build is not light (it routes to a
        // heavyweight path — the exact one depends on the surface words, but it must
        // never be the lean Light path).
        assert_ne!(
            classify("做一个简单的小商城,支持下单和支付"),
            TaskKind::Light
        );
    }

    #[test]
    fn light_classification_skips_research_docs_and_gates() {
        // The whole point of the downgrade: a Light-classified simple build plans
        // straight to spec -> implement -> verify, with NO research / docs / gates.
        let p = plan("做一个简单的待办清单单页应用,纯前端 HTML+CSS+JS,支持添加删除");
        assert_eq!(p.kind, TaskKind::Light);
        assert!(!p.includes(Phase::Research));
        assert!(!p.includes(Phase::Docs));
        assert!(!p.includes(Phase::DocsConfirm));
        assert!(!p.includes(Phase::PreviewConfirm));
        assert!(!p.includes(Phase::Delivery));
        // …but still plans the implementation + the quality (hard) gate.
        assert!(p.includes(Phase::Spec));
        assert!(p.includes(Phase::Frontend));
        assert!(p.includes(Phase::Quality));
    }

    #[test]
    fn classification_boundary_simple_vs_complex_samples() {
        // A contrast battery: each LEFT sample is genuinely small (Light); each
        // RIGHT sample is a real product (full pipeline). The classifier must split
        // them correctly — simple stays light, complex stays full.
        let light = [
            "做一个简单的倒计时单页应用",
            "写个静态网页放我的简历",
            "a small tool to convert markdown to html",
            "做一个极简的番茄钟,纯前端",
        ];
        for r in light {
            assert_eq!(classify(r), TaskKind::Light, "should be Light: {r}");
        }
        let heavy = [
            "做一个在线协作文档平台,带账号和实时同步",
            "构建一个电商网站,商品、购物车、支付、后台管理",
            "build a multi-page SaaS with authentication and a postgres database",
            "做一个带后端接口和数据库的待办应用,支持多用户登录",
        ];
        for r in heavy {
            assert_ne!(classify(r), TaskKind::Light, "should NOT be Light: {r}");
        }
    }

    #[test]
    fn bugfix_classification_for_broken_button() {
        // The task's required bug-fix boundary sample.
        assert_eq!(classify("修复登录按钮点击没反应"), TaskKind::Bugfix);
    }

    #[test]
    fn light_plan_is_the_lean_subset_no_gates() {
        // Whether reached by classification or forced via `plan_light`, a Light
        // plan skips research/docs/both gates/delivery and keeps spec+quality.
        for p in [plan("帮我改个文案"), plan_light("anything at all")] {
            assert_eq!(p.kind, TaskKind::Light);
            assert!(p.kind.is_light());
            assert!(p.includes(Phase::Spec));
            assert!(p.includes(Phase::Quality));
            assert!(!p.includes(Phase::Research));
            assert!(!p.includes(Phase::Docs));
            assert!(!p.includes(Phase::DocsConfirm));
            assert!(!p.includes(Phase::PreviewConfirm));
            assert!(!p.includes(Phase::Delivery));
        }
    }

    #[test]
    fn advisory_prior_is_explicitly_non_binding_per_kind() {
        // The Wave 3 demotion: the planner returns an ADVISORY prior the director
        // may ignore — never a forced phase chain. Every kind's hint must read as a
        // suggestion (the "advisory" / "may use or ignore" framing) and hand the
        // plan back to the director.
        for req in [
            "做一个电商平台",          // Greenfield
            "做一个前端落地页",        // FrontendOnly
            "写一个后端 graphql 接口", // BackendOnly
            "修复一个报错",            // Bugfix
            "重构 app.rs 拆分模块",    // Refactor
            "先写需求文档",            // DocsOnly
            "帮我改个文案",            // Light
        ] {
            let a = advisory_prior(req);
            let lower = a.to_lowercase();
            assert!(
                lower.contains("advisory"),
                "the prior is explicitly advisory for `{req}`: {a}"
            );
            assert!(
                lower.contains("yours to decide") || lower.contains("not a route you must"),
                "the prior hands the plan back to the director for `{req}`: {a}"
            );
            // It must NOT prescribe the literal fixed phase chain.
            assert!(
                !lower.contains("research -> docs") && !a.contains("研究→文档"),
                "the prior must not name a forced phase chain for `{req}`: {a}"
            );
        }
    }

    #[test]
    fn advisory_prior_reflects_the_classification() {
        // A greenfield prior mentions the full-build shape; a light prior mentions
        // doing it directly — so the director's read matches the goal's size.
        assert!(advisory_prior("做一个电商平台")
            .to_lowercase()
            .contains("full product"));
        assert!(advisory_prior("帮我改个文案")
            .to_lowercase()
            .contains("just do it"));
    }

    #[test]
    fn phase_from_id_parses_canonical_and_aliases() {
        assert_eq!(phase_from_id("frontend"), Some(Phase::Frontend));
        assert_eq!(phase_from_id("  FE "), Some(Phase::Frontend));
        assert_eq!(phase_from_id("backend"), Some(Phase::Backend));
        assert_eq!(phase_from_id("api"), Some(Phase::Backend));
        assert_eq!(phase_from_id("QA"), Some(Phase::Quality));
        assert_eq!(
            phase_from_id("preview-confirm"),
            Some(Phase::PreviewConfirm)
        );
        assert_eq!(phase_from_id("plan"), Some(Phase::Spec));
        // Every canonical id round-trips.
        for p in umadev_spec::PHASE_CHAIN {
            assert_eq!(phase_from_id(p.id()), Some(*p), "{}", p.id());
        }
        assert_eq!(phase_from_id("nonsense"), None);
        assert_eq!(phase_from_id(""), None);
    }

    #[test]
    fn plan_light_forces_light_for_any_requirement() {
        // `plan_light` ignores classification — even a greenfield ask is pinned
        // to Light when the user explicitly chose the fast track.
        let p = plan_light("做一个完整的电商平台");
        assert_eq!(p.kind, TaskKind::Light);
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
            "改个文案",
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

    // ---- derive_project_context ----------------------------------------

    #[test]
    fn context_lenient_for_simple_static_frontend() {
        // An explicitly-simple, pure-frontend build with no backend artifacts →
        // the lenient static-frontend context (surface rules skipped).
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = derive_project_context(
            "做一个简单的待办清单单页应用,纯前端 HTML+CSS+JS",
            tmp.path(),
            "todo",
        );
        assert!(
            ctx.static_frontend_only,
            "a proven static frontend should get the lenient context"
        );
    }

    #[test]
    fn context_strict_for_greenfield_product() {
        // A full product (Greenfield) always stays strict.
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = derive_project_context("做一个电商平台", tmp.path(), "shop");
        assert!(
            !ctx.static_frontend_only,
            "a greenfield product must stay strict"
        );
    }

    #[test]
    fn context_strict_when_requirement_has_heavy_signal() {
        // "简单的" (simple) present but a login + database veto the lenient path —
        // even if it classified frontend-ish, the heavy signal keeps it strict.
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx =
            derive_project_context("做一个简单的前端页面,带用户登录和数据库", tmp.path(), "app");
        assert!(
            !ctx.static_frontend_only,
            "auth/database words must keep governance strict"
        );
    }

    #[test]
    fn context_strict_when_arch_doc_declares_a_surface() {
        // A simple-frontend requirement, but the architecture doc grew an API
        // surface → strict (the doc proves a backend exists).
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("output");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            out.join("todo-architecture.md"),
            "# Architecture\n\n## API surface\n\n| Method | Path |\n|---|---|\n",
        )
        .unwrap();
        let ctx = derive_project_context("做一个简单的待办清单单页应用,纯前端", tmp.path(), "todo");
        assert!(
            !ctx.static_frontend_only,
            "an architecture doc with an API surface proves a backend → strict"
        );
    }

    #[test]
    fn context_stays_strict_when_architecture_input_exceeds_the_read_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let out = tmp.path().join("output");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            out.join("todo-architecture.md"),
            vec![b' '; MAX_PLANNER_DOC_BYTES + 1],
        )
        .unwrap();

        let ctx = derive_project_context("做一个简单的待办清单单页应用,纯前端", tmp.path(), "todo");
        assert!(
            !ctx.static_frontend_only,
            "unavailable architecture evidence must not disarm strict governance"
        );
    }

    #[cfg(unix)]
    #[test]
    fn context_stays_strict_for_a_symlinked_architecture_contract() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(workspace.path().join("output")).unwrap();
        let doc = outside.path().join("architecture.md");
        std::fs::write(&doc, "# Architecture\n").unwrap();
        symlink(&doc, workspace.path().join("output/todo-architecture.md")).unwrap();

        let ctx = derive_project_context(
            "做一个简单的待办清单单页应用,纯前端",
            workspace.path(),
            "todo",
        );
        assert!(!ctx.static_frontend_only);
    }

    #[test]
    fn context_strict_when_a_source_file_has_server_evidence() {
        // A simple-frontend requirement, but a produced source file boots a
        // server → strict (the project grew a backend).
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let listen = format!("{}.{}(3000)", "app", "listen");
        std::fs::write(
            src.join("server.ts"),
            format!("const app = express(); {listen};"),
        )
        .unwrap();
        let ctx = derive_project_context("做一个简单的待办清单单页应用,纯前端", tmp.path(), "todo");
        assert!(
            !ctx.static_frontend_only,
            "a produced server file proves a backend → strict"
        );
    }

    #[test]
    fn doc_surface_detection() {
        assert!(doc_declares_server_surface("## api surface\n| GET | /x |"));
        assert!(doc_declares_server_surface("uses a postgres database"));
        assert!(doc_declares_server_surface("需要鉴权"));
        assert!(!doc_declares_server_surface(""));
        assert!(!doc_declares_server_surface(
            "just a color palette and a few buttons"
        ));
    }

    #[test]
    fn lean_frontend_build_skips_the_empty_backend_phase() {
        // A simple pure-frontend page is Light, but should not pay for a
        // do-nothing Backend phase (~25% of the run was an empty backend turn).
        let p = plan("做一个简单的番茄钟计时器单页应用,纯前端,开始/暂停/重置");
        assert_eq!(p.kind, TaskKind::Light);
        assert!(
            p.includes(Phase::Frontend),
            "a frontend page keeps Frontend"
        );
        assert!(
            !p.includes(Phase::Backend),
            "a pure frontend page drops Backend"
        );
        assert!(p.includes(Phase::Spec) && p.includes(Phase::Quality));
    }

    #[test]
    fn lean_script_build_skips_the_empty_frontend_phase() {
        // The mirror: a small backend/script build drops the empty Frontend phase.
        let p = plan("写一个简单的脚本,读取 csv 文件统计行数");
        assert_eq!(p.kind, TaskKind::Light);
        assert!(!p.includes(Phase::Frontend), "a script drops Frontend");
        assert!(p.includes(Phase::Backend), "a script keeps Backend");
    }

    #[test]
    fn lean_ambiguous_build_keeps_both_surfaces() {
        // No clear one-sidedness → keep both (conservative, never strands work).
        let p = plan("做一个简单的小工具");
        assert_eq!(p.kind, TaskKind::Light);
        assert!(p.includes(Phase::Frontend) && p.includes(Phase::Backend));
    }

    #[cfg(unix)]
    #[test]
    fn governance_context_write_never_follows_managed_links() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let outside_context = outside.path().join("governance-context.json");
        std::fs::write(&outside_context, "outside").unwrap();

        symlink(outside.path(), root.path().join(".umadev")).unwrap();
        let _ = persist_project_context("build a dashboard", root.path(), "demo");
        assert_eq!(
            std::fs::read_to_string(&outside_context).unwrap(),
            "outside"
        );

        std::fs::remove_file(root.path().join(".umadev")).unwrap();
        std::fs::create_dir(root.path().join(".umadev")).unwrap();
        symlink(&outside_context, root.path().join(GOVERNANCE_CONTEXT_REL)).unwrap();
        let _ = persist_project_context("build a dashboard", root.path(), "demo");
        assert_eq!(
            std::fs::read_to_string(&outside_context).unwrap(),
            "outside"
        );
    }
}
