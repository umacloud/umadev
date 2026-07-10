//! Intelligent intent router (Wave 1, L1) — UmaDev's "thinking" primitive.
//!
//! The router is UmaDev borrowing the base's brain to DECIDE *how* to handle a
//! turn, before any work begins. It produces one typed [`RoutePlan`] the caller
//! reads to choose a path (fast / deliberate / clarify), size the team, and budget
//! the turn. It performs NO work itself and owns NO model — it consults the borrowed
//! brain over a read-only fork, exactly like the proven critic / intake patterns.
//!
//! ## The chat surface: the BRAIN judges intent ([`route_via_brain`])
//!
//! UmaDev depends on the base ecosystem — the base's own model IS the brain. So the
//! default chat surface routes a turn by ASKING THE BRAIN, not a keyword table:
//! [`route_via_brain`] runs one stateless `complete()` triage (`claude --print` and
//! equivalents — no fork, no session) and the brain decides
//! chat / explain / quick_edit / debug / build. A model judges "你能帮我做什么?" is a
//! greeting and "把标题改成 X" is a tweak far better than any word list could. There
//! is **no deterministic keyword classifier** on this path by design: if the brain
//! is unreachable the product can't run anyway, so a failed / garbage consult
//! degrades to the lightest path ([`RouteClass::Chat`], pass-through to the base),
//! never a keyword guess.
//!
//! ## The deterministic helpers (sizing + the explicit-run path)
//!
//! [`classify`] + [`looks_like_work_request`] + the fork-based [`route`] / [`reconcile`]
//! still exist to SIZE a build (kind / depth / team) and serve the explicit `/run`
//! path ([`for_run`], which already KNOWS the intent is a build). They are not the
//! chat surface's intent judge — the brain is.
//!
//! ## Invariants (mirror `critics.rs` / `director.rs`)
//!
//! 1. **Fail-open.** `session == None`, an offline brain, a fork that won't open, a
//!    consult that times out / returns garbage — every one of these degrades to the
//!    pure Tier-0 result. The router can NEVER block the host or return an error.
//! 2. **No new endpoint.** The Tier-1 consult runs over the SAME borrowed brain +
//!    its `fork()`; no extra model, no API key.
//! 3. **Read-only.** The consult runs on an isolated read-only fork that never
//!    touches the main writer session (single-writer preserved).
//! 4. **Observational.** Producing a [`RoutePlan`] changes nothing on disk; the
//!    caller decides what to do with it.

use std::collections::HashSet;

use umadev_runtime::BaseSession;

use crate::critics::Seat;
use crate::planner::{classify, TaskKind};
use crate::runner::RunOptions;

/// How a turn should be handled — the top-level routing decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteClass {
    /// Pure conversation — a greeting, an opinion, small talk. Fast path, no
    /// run-lock, light firmware.
    Chat,
    /// A "tell me about X" / "what does this do" answer — read-only explanation,
    /// no workspace mutation. Fast path.
    Explain,
    /// A small, well-scoped edit ("改个文案", "rename this var"). Fast single-writer
    /// turn + a targeted verify; no full team / gate machinery.
    QuickEdit,
    /// A defect to diagnose and fix. Fast when shallow, deliberate when the blast
    /// radius is unknown.
    Debug,
    /// A real build — a feature, a product, a non-trivial change. Deliberate path:
    /// run-lock, gates, team.
    Build,
}

impl RouteClass {
    /// Stable lowercase id for events / logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Explain => "explain",
            Self::QuickEdit => "quick_edit",
            Self::Debug => "debug",
            Self::Build => "build",
        }
    }

    /// Whether this class mutates the workspace (and therefore needs the
    /// single-writer run-lock). `Chat` / `Explain` are read-only.
    #[must_use]
    pub const fn mutates_workspace(self) -> bool {
        matches!(self, Self::QuickEdit | Self::Debug | Self::Build)
    }

    /// A coarse "how much machinery" rank used only for reconciliation (a brain
    /// verdict may RAISE this, never lower it below the Tier-0 floor).
    const fn rank(self) -> u8 {
        match self {
            Self::Chat => 0,
            Self::Explain => 1,
            Self::QuickEdit => 2,
            Self::Debug => 3,
            Self::Build => 4,
        }
    }
}

/// How much deliberation a turn warrants — orthogonal to [`RouteClass`] (a `Debug`
/// can be `Fast` or `Deep`). Drives whether the caller takes the deliberate path
/// and how large the team / budget is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Single-shot, no plan, no team — the cheapest path.
    Fast,
    /// A plan + a sized team + the gate machinery (the default for real work).
    Standard,
    /// Maximum deliberation — full team, full gates, the deepest plan.
    Deep,
}

impl Depth {
    /// Stable lowercase id for events / logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Deep => "deep",
        }
    }

    /// Whether this depth takes the deliberate (plan + gate + team) path.
    #[must_use]
    pub const fn is_deliberate(self) -> bool {
        matches!(self, Self::Standard | Self::Deep)
    }

    /// A **generous per-run turn ceiling** for a base session opened at this depth —
    /// a RUNAWAY BACKSTOP, not a tight work budget. Deeper work earns more turns; the
    /// caps are sized so a real build of this depth never truncates (the base reports
    /// hitting it as `error_max_turns` → `TurnStatus::Truncated`, and the deterministic
    /// floor is the real stop). A chat / quick-edit (`Fast`) gets the low cap, a
    /// deliberate build (`Standard` / `Deep`) a much higher one. This is the source of
    /// the optional `--max-turns` a caller threads into the claude session spawn; a
    /// read-only critic consult is capped even lower at the host layer. Deterministic.
    #[must_use]
    pub const fn max_turns(self) -> u32 {
        match self {
            // Chat / quick-edit / a fast single-page build: a few tool loops, not a saga.
            Self::Fast => 40,
            // A deliberate build (plan + team + gates): generous headroom for a real feature.
            Self::Standard => 150,
            // The deepest greenfield play: the most headroom before the backstop trips.
            Self::Deep => 400,
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Fast => 0,
            Self::Standard => 1,
            Self::Deep => 2,
        }
    }
}

/// A rough ceiling on what a turn should spend — surfaced so the user sees the
/// expected cost before the engine commits. Deterministic, derived from
/// class + depth; never a hard limit (the irreversible floor + idle watchdog are
/// the real bounds), just an expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Rough upper bound on base tool-calls for this turn.
    pub max_tool_calls: u32,
    /// Rough upper bound on tokens for this turn (worker generation budget).
    pub max_tokens: u32,
}

impl Budget {
    /// The deterministic budget for a class + depth — small for chat, generous for
    /// a deep build. Used only to set expectations; never enforced as a hard cap.
    #[must_use]
    pub fn for_route(class: RouteClass, depth: Depth) -> Self {
        let (calls, tokens) = match (class, depth) {
            (RouteClass::Chat | RouteClass::Explain, _) => (4, 4_000),
            (RouteClass::QuickEdit, _) => (20, 12_000),
            (RouteClass::Debug, Depth::Fast) => (40, 24_000),
            (RouteClass::Debug, _) => (80, 48_000),
            (RouteClass::Build, Depth::Fast) => (60, 32_000),
            (RouteClass::Build, Depth::Standard) => (160, 96_000),
            (RouteClass::Build, Depth::Deep) => (320, 192_000),
        };
        Self {
            max_tool_calls: calls,
            max_tokens: tokens,
        }
    }
}

/// One batched, multiple-choice clarification the router wants to ask BEFORE
/// committing — used only when the request is genuinely ambiguous in a way reading
/// the code can't resolve. Surfaced as ONE question with discrete options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClarifyQuestion {
    /// The single question to ask.
    pub question: String,
    /// Discrete answer options (an MCQ). May be empty for a free-form ask, but the
    /// router prefers options so the user just picks.
    pub options: Vec<String>,
}

/// The router's typed decision for one turn — the artifact UmaDev owns and the
/// caller reads to choose a path, size the team, and budget the work.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutePlan {
    /// How to handle the turn (chat / explain / quick-edit / debug / build).
    pub class: RouteClass,
    /// The task kind (reuses the planner's taxonomy) — feeds team sizing + the plan.
    pub kind: TaskKind,
    /// How much deliberation the turn warrants.
    pub depth: Depth,
    /// The seats to convene (doers serial, critics parallel — the caller decides).
    pub team: Vec<Seat>,
    /// Path hints — likely-relevant files / dirs the brain or keywords surfaced.
    /// Feeds repo-map + retrieval in later waves; advisory only.
    pub scope: Vec<String>,
    /// A batched clarification to ask before committing, when genuinely ambiguous.
    pub needs_clarify: Option<ClarifyQuestion>,
    /// Rough expected cost for this turn (expectation, not a hard cap).
    pub est_budget: Budget,
    /// The router's confidence in this plan, `0.0..=1.0`. Tier-0 alone is modest;
    /// a brain-assisted reconciliation raises it.
    pub confidence: f32,
}

impl RoutePlan {
    /// A one-line human rationale for this route — what UmaDev decided and why, for
    /// the [`crate::events::EngineEvent::IntentDecided`] card. Bilingual-friendly,
    /// derived deterministically from the typed fields (no model call).
    #[must_use]
    pub fn rationale(&self) -> String {
        match self.class {
            RouteClass::Chat => "这是对话,直接回应,不进开发流程。".to_string(),
            RouteClass::Explain => "这是一次讲解/答疑,只读理解,不改动工作区。".to_string(),
            RouteClass::QuickEdit => "这是一个小修改,快速单写 + 定向校验即可。".to_string(),
            RouteClass::Debug => {
                if self.depth.is_deliberate() {
                    "这是一个排障任务,影响面待定,进研发流程定位+修复+回归。".to_string()
                } else {
                    "这是一个小排障,快速定位并修复。".to_string()
                }
            }
            RouteClass::Build => {
                // A REASON (why build), not a restatement of the localized
                // intent.build headline the card already shows - otherwise the card
                // printed the full-build line twice (the reported duplicate).
                "判定为完整构建:需求规模较大、涉及多个环节,交由多角色团队分阶段交付更稳妥。"
                    .to_string()
            }
        }
    }

    /// The **generous turn ceiling** for a base session driving this route — the
    /// optional `--max-turns` runaway backstop a caller may thread into the session
    /// spawn. Derived from the route's [`Depth`] (see [`Depth::max_turns`]): a
    /// deliberate build gets a higher cap than a chat / quick-edit. Never a tight
    /// leash — sized so real work never truncates. Deterministic; no model call.
    #[must_use]
    pub const fn max_turns(&self) -> u32 {
        self.depth.max_turns()
    }
}

/// Route ONE turn — produce the typed [`RoutePlan`] the caller drives off.
///
/// `session`: the live base session to (read-only) fork for the Tier-1 consult, or
/// `None` (CLI / offline / no brain) to run pure Tier-0. `options` carries the run
/// context (model, trust mode). `requirement` is the user's message this turn.
///
/// **Fail-open by contract:** any failure at any point — no session, an offline
/// brain, a fork that won't open, a timed-out / unparseable consult — yields the
/// pure Tier-0 deterministic [`RoutePlan`]. This function never returns an error and
/// never blocks the host.
/// The route for an EXPLICIT build entry (`/run`, `Action::StartRun`, the CLI
/// `run` verb). The user invoked the build command, so the **class is known to be
/// `Build`** — we do NOT re-derive intent and risk second-guessing a clear build
/// into a `QuickEdit`/`Chat`. Tier-0 still sizes the *kind / depth / team* from the
/// text (a single page is a `Fast` build; a full product is `Standard`/`Deep`), but
/// the class is forced to `Build` so the director always synthesizes and shows a
/// plan. Deterministic — no fork, no latency.
#[must_use]
pub fn for_run(requirement: &str) -> RoutePlan {
    let mut r = tier0(requirement);
    // Force the known class; re-size the team for a Build of this kind/depth if
    // Tier-0 had sized a non-build (e.g. it read the text as Chat/Explain).
    if r.class != RouteClass::Build {
        r.class = RouteClass::Build;
        // A bare/odd requirement under an explicit run still builds — never below
        // Fast (the depth is left proportional), but ensure a real build team rather
        // than a chat one.
        r.team = tier0_team(r.kind, RouteClass::Build, r.depth, requirement);
        r.est_budget = Budget::for_route(RouteClass::Build, r.depth);
    }
    r
}

/// Route a turn: run the deterministic Tier-0 floor, then (when a `session` is
/// given) refine with a fail-open Tier-1 brain consult on a read-only fork. The
/// brain may escalate but never drops below the safe floor. Returns the Tier-0
/// result on `None` / any consult failure.
pub async fn route(
    session: Option<&mut dyn BaseSession>,
    options: &RunOptions,
    requirement: &str,
) -> RoutePlan {
    // Tier-0 ALWAYS runs first — it is the floor and the fallback.
    let floor = tier0(requirement);

    // No brain to consult → the deterministic floor is the answer.
    let Some(session) = session else {
        return floor;
    };

    // Tier-1: a brain-assisted consult on a read-only fork. Fail-open: a `None`
    // (no fork / offline / timeout / garbage) leaves the floor untouched.
    match consult_route(session, options, requirement).await {
        Some(brain) => reconcile(&floor, &brain, requirement),
        None => floor,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Tier-0 — deterministic, zero-latency floor + fallback
// ───────────────────────────────────────────────────────────────────────────

/// The deterministic route: classify the kind (the existing planner table), map it
/// to a class + depth, and size a team. Always complete, always safe — this is what
/// the router returns when there's no brain or the brain consult fails.
fn tier0(requirement: &str) -> RoutePlan {
    let kind = classify(requirement);
    let is_work = looks_like_work_request(requirement);

    // Map (kind, is_work) → the conservative class/depth FLOOR. The floor never
    // over-commits: an ambiguous "看看这个" stays Explain, not Build, and the brain
    // (Tier-1) may escalate it — but a keyword-flagged real build starts at Build.
    let (class, depth) = floor_class_depth(kind, is_work, requirement);
    let team = tier0_team(kind, class, depth, requirement);
    let scope = path_hints_from_text(requirement);
    RoutePlan {
        class,
        kind,
        depth,
        team,
        scope,
        needs_clarify: None,
        est_budget: Budget::for_route(class, depth),
        // Tier-0 alone is a modest-confidence heuristic; a clear greeting / clear
        // build is higher, an ambiguous middle is lower (so the caller knows the
        // brain would help). All deterministic.
        confidence: tier0_confidence(kind, is_work),
    }
}

/// Whether the text asks to CREATE a new thing (a build) vs EDIT an existing one.
/// Deterministic verb heuristic; the brain (Tier-1) refines it. Used to split an
/// ambiguous `Light` kind between a small Build and a QuickEdit.
fn is_create_request(requirement: &str) -> bool {
    let q = requirement.to_lowercase();
    const CREATE: &[&str] = &[
        "做一个",
        "做个",
        "做一款",
        "建一个",
        "搭一个",
        "搭个",
        "写一个",
        "写个",
        "新建",
        "开发一个",
        "开发个",
        "生成一个",
        "实现一个",
        "来一个",
        "整一个",
        "create",
        "build",
        "make a",
        "make me",
        "scaffold",
        "generate a",
        "implement a",
        "build me",
        "set up a",
        "new app",
        "new project",
        "new page",
    ];
    // A bare "做/建/写 + a noun-ish thing" also counts, but keep the floor cautious:
    // require one of the explicit create phrases. An edit ("改/修改/调整/rename/把…改成")
    // has none of these → QuickEdit.
    CREATE.iter().any(|v| q.contains(v))
}

/// Map the planner's [`TaskKind`] + a work-class signal to the conservative
/// (class, depth) floor. Deterministic and intentionally cautious: it never routes
/// to a heavier class than the keywords justify (the brain may escalate later).
fn floor_class_depth(kind: TaskKind, is_work: bool, requirement: &str) -> (RouteClass, Depth) {
    // Empty / whitespace → chat (nothing to do).
    if requirement.trim().is_empty() {
        return (RouteClass::Chat, Depth::Fast);
    }
    // A non-work message (greeting / opinion / chit-chat) is Chat.
    if !is_work {
        return (RouteClass::Chat, Depth::Fast);
    }
    match kind {
        // A real product / greenfield build → the deliberate path.
        TaskKind::Greenfield => (RouteClass::Build, Depth::Standard),
        // Front/back-only feature builds → Build, but lighter than a full product.
        TaskKind::FrontendOnly | TaskKind::BackendOnly => (RouteClass::Build, Depth::Standard),
        // A bugfix is a Debug; shallow by default (the blast radius is usually one
        // file), the brain may deepen it.
        TaskKind::Bugfix => (RouteClass::Debug, Depth::Fast),
        // A refactor is a small structured build — QuickEdit-ish but with verify.
        TaskKind::Refactor => (RouteClass::QuickEdit, Depth::Standard),
        // Docs/research only → an Explain-class read+write (no run-lock heaviness).
        TaskKind::DocsOnly => (RouteClass::Explain, Depth::Fast),
        // A `Light` kind is ambiguous between a small BUILD ("做一个简单的待办单页"
        // — create a new thing) and a small EDIT ("改个文案" — tweak existing code).
        // Disambiguate by intent verb: a create request is a fast Build (gets the
        // build path + a short visible plan); otherwise it's a QuickEdit (a fast
        // single-writer edit, no plan). Both are Fast — proportional, no heavy
        // process. (Tier-1's brain refines this when it's available.)
        TaskKind::Light => {
            // A doc artifact (README / changelog / a single markdown doc) is a quick
            // file write, NOT a product build — keep it on the QuickEdit path (a fast
            // single-writer turn, no plan synthesis, no team) even though it
            // "creates" a file. Only a NON-doc create (a tiny UI page / app) becomes a
            // fast Build, which gets the short visible plan and the minimal UI review
            // when it actually ships UI (see `build_ships_ui`).
            if is_create_request(requirement) && !crate::planner::is_doc_task(requirement) {
                (RouteClass::Build, Depth::Fast)
            } else {
                (RouteClass::QuickEdit, Depth::Fast)
            }
        }
    }
}

/// Tier-0 team for a (kind, class, depth, requirement). Reuses the planner's
/// complexity sense: a fast/light turn convenes NO team; a standard/deep build
/// convenes the seats the kind needs. Deterministic; the brain may widen it during
/// reconciliation.
fn tier0_team(kind: TaskKind, class: RouteClass, depth: Depth, requirement: &str) -> Vec<Seat> {
    // Pure conversation / read-only explanation → no team (it's overhead there).
    if matches!(class, RouteClass::Chat | RouteClass::Explain) {
        return Vec::new();
    }
    if matches!(class, RouteClass::Build) {
        if depth == Depth::Fast {
            // A Fast build earns the MINIMAL UI review core (designer + frontend + QA)
            // ONLY when it actually ships a user-facing UI surface — a chat-built page
            // IS a delivery and its UI/quality must be reviewed (the audit caught a
            // Fast build convening ZERO critics → a landing page shipped un-reviewed).
            // A Fast build that ships NO UI — a README / changelog / doc, a small
            // script, a tiny non-UI change — is NOT a UI delivery and convenes NO team:
            // reviewing a README with a designer + frontend + QA is pure token waste
            // (the user-reported "generating a README runs a full review" case). The
            // full kind-sized roster stays on a deliberate build below.
            if build_ships_ui(kind, requirement) {
                return vec![Seat::UiuxDesigner, Seat::FrontendEngineer, Seat::QaEngineer];
            }
            return Vec::new();
        }
        return Seat::team_for_kind(kind);
    }
    // QuickEdit / Debug: a team only when deliberate (an edit / shallow diagnose is
    // not a delivery that needs the roster).
    if depth == Depth::Fast {
        return Vec::new();
    }
    Seat::team_for_kind(kind)
}

/// Whether a Fast BUILD actually ships a user-facing UI surface — the signal that
/// decides if it earns the minimal UI review core ([`tier0_team`]). The UI-bearing
/// kinds (`Greenfield` / `FrontendOnly`) always do; a `Light` build does ONLY when it
/// names a frontend surface AND is not a documentation artifact (a README / changelog
/// / doc ships no UI). Everything else (a backend / script build, a bugfix, a refactor,
/// a docs task) ships no UI and convenes no review team on the Fast path. Deterministic.
fn build_ships_ui(kind: TaskKind, requirement: &str) -> bool {
    match kind {
        TaskKind::Greenfield | TaskKind::FrontendOnly => true,
        TaskKind::Light => {
            crate::planner::mentions_ui_surface(requirement)
                && !crate::planner::is_doc_task(requirement)
        }
        TaskKind::BackendOnly | TaskKind::Bugfix | TaskKind::Refactor | TaskKind::DocsOnly => false,
    }
}

/// A deterministic confidence for the Tier-0 verdict: high at the clear poles
/// (obvious greeting, obvious greenfield), lower in the ambiguous middle so the
/// caller can tell the brain consult is worth more there. `0.0..=1.0`.
fn tier0_confidence(kind: TaskKind, is_work: bool) -> f32 {
    if !is_work {
        return 0.8; // a clear non-work message is a confident Chat
    }
    match kind {
        TaskKind::Greenfield => 0.7,
        TaskKind::Bugfix | TaskKind::Refactor | TaskKind::DocsOnly => 0.6,
        TaskKind::FrontendOnly | TaskKind::BackendOnly => 0.55,
        TaskKind::Light => 0.5,
    }
}

/// A bilingual work-request detector — does the message ask to read, inspect,
/// explain, debug, review, change, or BUILD something (vs pure conversation)?
///
/// Ported into the agent crate (the TUI has an equivalent it uses for prompt
/// gating) so the router is self-contained. Deliberately broad + fail-open: a false
/// positive merely routes a chatty message as light work; a false negative leaves
/// it as Chat. Never blocks anything.
#[must_use]
pub fn looks_like_work_request(text: &str) -> bool {
    const EN: &[&str] = &[
        "build",
        "create",
        "make",
        "add",
        "implement",
        "write",
        "code",
        "fix",
        "debug",
        "refactor",
        "change",
        "modify",
        "update",
        "edit",
        "rewrite",
        "rename",
        "remove",
        "delete",
        "replace",
        "review",
        "audit",
        "inspect",
        "analyze",
        "analyse",
        "explain",
        "read",
        "look at",
        "check",
        "test",
        "run",
        "deploy",
        "optimize",
        "optimise",
        "improve",
        "design",
        "generate",
        "scaffold",
        "set up",
        "setup",
        "configure",
        "install",
        "render",
        "feature",
        "component",
        "endpoint",
        "api",
        "bug",
        "error",
        "crash",
        "function",
        "module",
        "page",
    ];
    const ZH: &[&str] = &[
        "做",
        "建",
        "创建",
        "实现",
        "写",
        "加",
        "新增",
        "增加",
        "修",
        "修复",
        "改",
        "修改",
        "更新",
        "重构",
        "删",
        "删除",
        "移除",
        "替换",
        "重命名",
        "审",
        "审查",
        "审核",
        "分析",
        "解释",
        "说明",
        "读",
        "看一下",
        "看看",
        "查",
        "检查",
        "测试",
        "运行",
        "跑",
        "部署",
        "优化",
        "改进",
        "设计",
        "生成",
        "搭建",
        "配置",
        "安装",
        "渲染",
        "功能",
        "组件",
        "接口",
        "页面",
        "报错",
        "错误",
        "崩溃",
        "函数",
        "模块",
        "帮我",
        "给我",
    ];
    let t = text.to_lowercase();
    if EN.iter().any(|k| t.contains(k)) {
        return true;
    }
    ZH.iter().any(|k| text.contains(k))
}

/// Cheap deterministic path hints — pull obvious file-ish tokens out of the
/// requirement (anything with a path separator or a known source extension). These
/// are advisory `scope` hints for later retrieval; an empty result is fine.
fn path_hints_from_text(text: &str) -> Vec<String> {
    const EXTS: &[&str] = &[
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".css", ".html", ".json",
        ".toml", ".md", ".vue", ".svelte", ".sql",
    ];
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '`')) {
        let tok = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | ':' | '.' | '!' | '?'));
        if tok.is_empty() {
            continue;
        }
        let looks_pathy = tok.contains('/') || EXTS.iter().any(|e| tok.to_lowercase().ends_with(e));
        if looks_pathy && seen.insert(tok.to_string()) {
            out.push(tok.to_string());
            if out.len() >= 8 {
                break;
            }
        }
    }
    out
}

// ───────────────────────────────────────────────────────────────────────────
// Tier-1 — brain-assisted consult (read-only fork) + reconciliation
// ───────────────────────────────────────────────────────────────────────────

/// The brain's structured opinion of a request. Every field is optional / tolerant
/// so a partial reply still parses (fail-open: a missing field falls back to the
/// Tier-0 prior during reconciliation).
/// Tolerant deserializer for the brain-triage array fields: accepts a JSON array of
/// strings, a SINGLE string (LLMs routinely collapse a one-element array to a scalar),
/// or anything else -> empty. Without this, one scalar-collapsed field makes
/// serde_json::from_str::<BrainRoute> fail the WHOLE struct, silently degrading a real
/// build to Chat on the primary chat surface (no plan/team/gates).
fn de_string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => vec![s],
        serde_json::Value::Array(a) => a
            .into_iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    })
}

/// Tolerant deserializer for confidence: accepts a JSON number, a quoted number
/// ("0.9" - LLMs routinely quote numbers), or an absent/other value -> 0.0. Without this,
/// one quoted confidence would fail the WHOLE BrainRoute parse (serde default only covers
/// an ABSENT field, not a present wrong-typed one) and silently degrade a real build to
/// Chat - the same class the array-field tolerance already guards against.
fn de_lenient_f32<'de, D>(d: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) as f32,
        serde_json::Value::String(s) => s.trim().parse::<f32>().unwrap_or(0.0),
        _ => 0.0,
    })
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
struct BrainRoute {
    /// `chat | explain | quick_edit | debug | build` (free text; mapped tolerantly).
    #[serde(default)]
    class: String,
    /// `greenfield | frontend_only | backend_only | bugfix | refactor | docs_only | light`.
    #[serde(default)]
    kind: String,
    /// `simple | medium | complex` — maps to a depth.
    #[serde(default)]
    complexity: String,
    /// What the request needs (roles / capabilities) — informs the team.
    #[serde(default, deserialize_with = "de_string_or_vec")]
    needs: Vec<String>,
    /// Likely-relevant files / dirs.
    #[serde(default, deserialize_with = "de_string_or_vec")]
    scope: Vec<String>,
    // NB: the prompt also invites a `risks` array; the router doesn't surface risks
    // (that's the plan's job — see `plan_state`), so it's intentionally not a field
    // here. serde ignores the unknown key, keeping the brain's schema unchanged.
    /// A clarifying question, when the request is genuinely ambiguous.
    #[serde(default)]
    clarify_question: String,
    /// Discrete options for the clarifying question.
    #[serde(default, deserialize_with = "de_string_or_vec")]
    clarify_options: Vec<String>,
    /// The brain's confidence `0.0..=1.0` (tolerant: out-of-range is clamped).
    #[serde(default, deserialize_with = "de_lenient_f32")]
    confidence: f32,
}

/// Run ONE strict-JSON routing consult on a read-only fork of `session`. Cloned
/// from the critic team's [`crate::continuous::ForkConsult`] mechanism — same
/// fork → judge-turn → parse path, same fail-open contract. Returns `None` on any
/// failure (no fork / offline / timeout / unparseable), which the caller treats as
/// "use the Tier-0 floor".
/// The intent-triage instruction the borrowed brain answers — shared by the
/// fork-based [`consult_route`] and the one-shot [`route_via_brain`].
const ROUTER_TRIAGE_SYSTEM: &str =
    "You are a senior engineering director triaging ONE incoming request before \
     any work starts. Decide how to handle it. Be decisive and terse. \
     `class`: chat (small talk / a greeting / a question about you) | explain (read-only \
     Q&A about code) | quick_edit (a small, well-scoped change to existing text/code) | \
     debug (diagnose+fix a defect) | build (create a real feature/product). A greeting or \
     a 'what can you do' question is chat, NOT build, even if it mentions building. \
     Conversely, a request to implement a WHOLE project / product / app — especially \
     one that points at a requirements / spec / PRD / 需求 / design document (e.g. \
     'build what's in docs/spec.md', '实现 docs 里的需求', 'do this project') — is \
     `build` with `kind:greenfield`, NEVER `quick_edit`, even if phrased tersely or as \
     just a file path: delivering a product from a spec is real, multi-part work. \
     Reserve `quick_edit` for a SMALL, single-surface change to something that already \
     exists. \
     Distinct from BOTH of the above: a request to WRITE / PRODUCE a DOCUMENT as the \
     deliverable ITSELF — a PRD, a spec, a design doc, a technical proposal, a research \
     / status report, a 需求文档 / 技术方案 / 设计文档 / 调研报告 / 周报 — is \
     `kind:docs_only` with `complexity:simple`: the output is a written document, NOT a \
     built product, so it wants ONE editorial pass (does it serve the requirement, is it \
     coherent and complete), never a full delivery team or a source-code build. This is \
     the OPPOSITE of 'build the product DESCRIBED IN a document' above — WRITING the spec \
     is a light `docs_only` task; IMPLEMENTING what the spec describes is \
     `build`/`greenfield`. Do not size a document up to a product just because it is \
     long or detailed. \
     `kind`: greenfield | frontend_only | backend_only | bugfix | refactor | docs_only | \
     light. `complexity`: simple | medium | complex. Only set `clarify_question` when the \
     request is genuinely ambiguous in a way you could NOT resolve by reading the code — \
     never ask what you can discover yourself. JSON shape: \
     {\"class\":\"…\",\"kind\":\"…\",\"complexity\":\"simple|medium|complex\",\
     \"needs\":[\"…\"],\"scope\":[\"file/dir\",…],\"risks\":[\"…\"],\
     \"clarify_question\":\"\",\"clarify_options\":[],\"confidence\":0.0}";

async fn consult_route(
    session: &mut dyn BaseSession,
    _options: &RunOptions,
    requirement: &str,
) -> Option<BrainRoute> {
    let user = format!("Request:\n{requirement}");

    // Fork a read-only session (bounded handshake) and run one strict-JSON judge
    // turn over it — reusing the exact ForkConsult mechanism the critic team uses.
    let fork = crate::continuous::fork_with_timeout(session).await;
    let consult = crate::continuous::ForkConsult::new(fork);
    let json_text = consult
        .judge_json("router", ROUTER_TRIAGE_SYSTEM, user)
        .await;
    consult.end().await;

    let text = json_text?;
    serde_json::from_str::<BrainRoute>(&text).ok()
}

/// Route a turn by asking the **borrowed brain** to classify the intent — a single
/// stateless one-shot consult (`claude --print` and equivalents), no fork, no
/// session lifecycle. This is the router for the chat surface: UmaDev depends on
/// the base ecosystem, so the base's own model is the judge of "chat vs. a small
/// edit vs. a real build" — far better than any keyword table. There is
/// intentionally **no deterministic keyword classifier** in this path: if the
/// brain is unreachable the product cannot run anyway, and a failed / garbage
/// consult degrades to the lightest path ([`RouteClass::Chat`]) so the turn still
/// reaches the base (which will surface any real connectivity error itself).
///
/// `runtime` is a freshly-built brain (`build_brain`) the caller owns; this fn does
/// not mutate the workspace and opens no session.
pub async fn route_via_brain(
    runtime: &dyn umadev_runtime::Runtime,
    requirement: &str,
) -> RoutePlan {
    match consult_brain_oneshot(runtime, requirement).await {
        Some(brain) => brain_to_route(&brain, requirement),
        // Simplest possible degradation (NOT a keyword fallback): treat it as a
        // chat turn and pass it straight to the base. This path is reached only
        // after `consult_brain_oneshot` already retried a prose (non-JSON) reply
        // once with a stricter JSON-only ask — so a real build whose first reply
        // was narrated still has a chance to route correctly before we fall back to
        // Chat. The Chat default here is a DELIBERATE design choice (UmaDev depends
        // on the base ecosystem; if the brain is truly unreachable the product can't
        // run anyway, so we never guess intent from a keyword list).
        None => brain_unavailable_chat_route(),
    }
}

/// One stateless `complete()` triage call on the borrowed brain. `None` on offline
/// / empty input / a call error / unparseable JSON.
///
/// On a first reply that yields no parseable JSON object (the brain answered in
/// prose), retry ONCE with a stricter "ONLY a JSON object, no prose" instruction
/// before giving up. A model that narrates intent is exactly the case where a build
/// would otherwise silently degrade to Chat (see [`brain_unavailable_chat_route`]);
/// the cheap second ask recovers it. Still fully fail-open — both calls returning
/// nothing usable leaves the caller on the lightest (Chat) path by design.
async fn consult_brain_oneshot(
    runtime: &dyn umadev_runtime::Runtime,
    requirement: &str,
) -> Option<BrainRoute> {
    if runtime.is_offline() || requirement.trim().is_empty() {
        return None;
    }
    // First ask: the standard triage system prompt.
    if let Some(parsed) = triage_once(runtime, ROUTER_TRIAGE_SYSTEM, requirement).await {
        return Some(parsed);
    }
    // Retry once, harder: a prose reply (no JSON) on a real build would otherwise
    // degrade to Chat — re-ask demanding a bare JSON object only.
    let strict_system = format!(
        "{ROUTER_TRIAGE_SYSTEM}\n\nIMPORTANT: Reply with EXACTLY ONE JSON object and \
         NOTHING ELSE — no markdown, no code fence, no prose, no explanation."
    );
    triage_once(runtime, &strict_system, requirement).await
}

/// One stateless triage round-trip: send `system` + the requirement, extract the
/// JSON object, parse it. `None` on a call error / no JSON / unparseable JSON.
async fn triage_once(
    runtime: &dyn umadev_runtime::Runtime,
    system: &str,
    requirement: &str,
) -> Option<BrainRoute> {
    let prompt = crate::experts::Prompt {
        system: system.to_string(),
        user: format!("Request:\n{requirement}"),
    };
    let resp = runtime
        .complete(prompt.into_request(String::new(), 400))
        .await
        .ok()?;
    let json = crate::continuous::extract_json_object(&resp.text)?;
    serde_json::from_str::<BrainRoute>(&json).ok()
}

/// Map the brain's triage verdict into an owned [`RoutePlan`] — the brain is
/// **authoritative** here (no escalate-only flooring): it decides the class, the
/// depth (from complexity), the kind, and the implied team. An unparseable field
/// falls back to the lightest sensible value.
fn brain_to_route(brain: &BrainRoute, requirement: &str) -> RoutePlan {
    let class = parse_class(&brain.class).unwrap_or(RouteClass::Chat);
    // Default kind: a mutating class (build / quick-edit / debug) whose `kind` field
    // is unparseable must NOT fall back to `Light` — `Light` convenes ZERO team, so a
    // brain that says "build, complex" but garbles `kind` would silently lose the
    // delivery roster (a deliberate build with no critics). Default a mutating class
    // to a BUILD-SHAPED kind (`Greenfield` → the full roster via `team_for_kind`); a
    // read-only class (chat / explain) keeps the light `Light` default (no team
    // wanted there anyway). The brain may still narrow it via a parseable `kind`.
    let mut kind = parse_kind(&brain.kind).unwrap_or_else(|| {
        if class.mutates_workspace() {
            TaskKind::Greenfield
        } else {
            TaskKind::Light
        }
    });
    // Domain floor for the TEAM (not the intent): a brain that sized a BUILD as the broad
    // `Greenfield` - the default / lazy pick, especially from a weaker third-party model - but
    // whose requirement is a PURE BACKEND task should scope to BackendOnly so it does not
    // convene irrelevant UI reviewers (the reported "优化后端代码 pulls in a uiux-designer +
    // frontend-engineer" waste). This does NOT re-route intent - the brain still decided BUILD.
    //
    // ONE-DIRECTION ONLY (Greenfield -> BackendOnly). We deliberately do NOT narrow
    // Greenfield -> FrontendOnly: a full-stack app (a blog, a shop, a dashboard) is almost
    // always described purely by its PAGES ("博客系统,有文章列表和文章详情页面") with no
    // backend keyword, so `classify` reads FrontendOnly on mere UI-keyword presence — but the
    // app STILL needs persistence. Narrowing it would DROP the backend phase entirely and
    // ship a frontend-only shell with no data layer, overruling the brain's authoritative
    // greenfield on a weak heuristic. A genuinely frontend-only build is caught upstream (the
    // brain routes it as a lean kind, or `classify`'s simple-build/`纯前端` path → Light).
    // This also makes `brain_to_route` consistent with the fork-based `reconcile` path, which
    // already trusts the brain's greenfield over a frontend-leaning floor.
    if class == RouteClass::Build
        && kind == TaskKind::Greenfield
        && crate::planner::classify(requirement) == TaskKind::BackendOnly
    {
        kind = TaskKind::BackendOnly;
    }
    // Depth from the brain's complexity. A garbled/missing `complexity` on a real
    // product BUILD must NOT default to `Fast`: a Fast build is non-deliberate
    // (`Depth::Fast.is_deliberate() == false`) AND `reconcile_team` early-returns the
    // EMPTY team on Fast — so a brain reply `{class:build, kind:frontend_only,
    // complexity:""}` would ship with NO UI review team and SKIP the plan+acceptance
    // floor, while the SAME text via `/run` (`for_run` → `tier0_team` →
    // `build_ships_ui`) gets a 3-seat UI review + the deliberate gate. Floor a
    // PRODUCT-kind build (greenfield / frontend-only / backend-only — exactly the kinds
    // the deterministic floor sizes to `Standard`, see `floor_class_depth`) to at least
    // `Standard`. Light/docs/bugfix/refactor builds keep the brain's depth — a doc
    // write or a tiny page is proportional Fast work by design (the deterministic floor
    // keeps those Fast too), so we never over-deepen them.
    let parsed_depth = parse_depth(&brain.complexity).unwrap_or(Depth::Fast);
    let is_product_build = class == RouteClass::Build
        && matches!(
            kind,
            TaskKind::Greenfield | TaskKind::FrontendOnly | TaskKind::BackendOnly
        );
    let depth = if is_product_build && parsed_depth.rank() < Depth::Standard.rank() {
        Depth::Standard
    } else {
        parsed_depth
    };
    // Team via the SAME deterministic sizing the explicit `/run` path uses
    // (`tier0_team`, which carries the `build_ships_ui` rescue so a Fast UI build still
    // earns the minimal designer+frontend+QA review) — instead of
    // `reconcile_team(&[], …)`, which early-returns EMPTY on a Fast depth and would drop
    // the roster. A chat-surface build then gets the identical review roster as `/run`
    // for the same input. The brain's explicit `needs` may only WIDEN it (never shrink).
    let mut team = tier0_team(kind, class, depth, requirement);
    // The brain's explicit `needs` may only WIDEN a real review roster — never seat a
    // team on a turn that convenes none. A Chat/Explain turn (and any Fast-depth
    // quick-edit/debug) has an EMPTY floor team on purpose, and widening it would
    // mis-frame the firmware persona (`context.rs` reads `route.team.first()` to inject a
    // seat persona) and could convene an unwanted critic. Guard the widening exactly like
    // the fork path's `reconcile_team` does, so both router surfaces agree.
    if !matches!(class, RouteClass::Chat | RouteClass::Explain) && depth != Depth::Fast {
        let mut seen: HashSet<Seat> = team.iter().copied().collect();
        for n in &brain.needs {
            if let Some(s) = Seat::from_alias(n) {
                if seen.insert(s) {
                    team.push(s);
                }
            }
        }
    }
    let scope = union_scope(&[], &brain.scope);
    let needs_clarify = build_clarify(brain);
    RoutePlan {
        class,
        kind,
        depth,
        team,
        scope,
        needs_clarify,
        est_budget: Budget::for_route(class, depth),
        confidence: brain.confidence.clamp(0.0, 1.0),
    }
}

/// The lightest route — used only when the brain can't be reached (no keyword
/// guessing): the turn is treated as chat and handed to the base as-is.
fn brain_unavailable_chat_route() -> RoutePlan {
    RoutePlan {
        class: RouteClass::Chat,
        kind: TaskKind::Light,
        depth: Depth::Fast,
        team: Vec::new(),
        scope: Vec::new(),
        needs_clarify: None,
        est_budget: Budget::for_route(RouteClass::Chat, Depth::Fast),
        confidence: 0.3,
    }
}

/// Reconcile the brain's opinion with the deterministic Tier-0 floor under ONE
/// rule: the brain may **escalate** (raise the class rank, deepen, widen the team,
/// add a clarification) but may **never drop below the safe floor** — it cannot
/// silently downgrade a request the keywords flagged as real work into chat.
fn reconcile(floor: &RoutePlan, brain: &BrainRoute, requirement: &str) -> RoutePlan {
    // Map the brain's free-text fields tolerantly; an unrecognised value falls back
    // to the floor's value, so a garbage field is simply ignored.
    let brain_class = parse_class(&brain.class).unwrap_or(floor.class);
    let brain_depth = parse_depth(&brain.complexity).unwrap_or(floor.depth);

    // ESCALATE-ONLY: take the HIGHER of (floor, brain) on both axes. The brain can
    // make a turn heavier (a "simple change" the brain sees is actually a refactor),
    // never lighter than the keyword floor demanded.
    let class = if brain_class.rank() >= floor.class.rank() {
        brain_class
    } else {
        floor.class
    };
    let depth = if brain_depth.rank() >= floor.depth.rank() {
        brain_depth
    } else {
        floor.depth
    };

    // Kind: prefer the brain's read when it parses (it reflects the same taxonomy
    // and is usually a better reading of intent), else keep the floor's.
    let kind = parse_kind(&brain.kind).unwrap_or(floor.kind);

    // Team: union of the floor team and the brain-implied team (sized by the
    // reconciled kind/depth), so escalation can only ADD seats, never remove the
    // floor's. A fast/chat turn still gets no team.
    let team = reconcile_team(&floor.team, kind, class, depth, &brain.needs);

    // Scope: union of the floor's path hints + the brain's scope (deduped, bounded).
    let scope = union_scope(&floor.scope, &brain.scope);

    // Clarify: honour the brain's batched MCQ when present + non-empty.
    let needs_clarify = build_clarify(brain);

    // Confidence: the higher of the two, clamped — a brain-reconciled route is at
    // least as confident as the floor, and the brain's own confidence can raise it.
    let confidence = floor
        .confidence
        .max(brain.confidence.clamp(0.0, 1.0))
        .clamp(0.0, 1.0);

    let _ = requirement; // reserved for future scope-from-text fusion
    RoutePlan {
        class,
        kind,
        depth,
        team,
        scope,
        needs_clarify,
        est_budget: Budget::for_route(class, depth),
        confidence,
    }
}

/// Reconcile the team: start from the floor's seats, add the seats the reconciled
/// (kind/class/depth) implies, plus any seat the brain's `needs` names. Escalation
/// can only widen the team. A Chat/Explain/Fast turn keeps no team.
fn reconcile_team(
    floor_team: &[Seat],
    kind: TaskKind,
    class: RouteClass,
    depth: Depth,
    needs: &[String],
) -> Vec<Seat> {
    if matches!(class, RouteClass::Chat | RouteClass::Explain) || depth == Depth::Fast {
        // Even here, if the floor had a team (it shouldn't on a fast turn) keep it —
        // we never drop the floor. But a fast/chat floor team is empty by design.
        return floor_team.to_vec();
    }
    let mut seen: HashSet<Seat> = floor_team.iter().copied().collect();
    let mut out: Vec<Seat> = floor_team.to_vec();
    for s in Seat::team_for_kind(kind) {
        if seen.insert(s) {
            out.push(s);
        }
    }
    for n in needs {
        if let Some(s) = Seat::from_alias(n) {
            if seen.insert(s) {
                out.push(s);
            }
        }
    }
    out
}

/// Union two scope lists (floor first), deduped, bounded to 12 entries.
fn union_scope(floor: &[String], brain: &[String]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for s in floor.iter().chain(brain.iter()) {
        let t = s.trim();
        if !t.is_empty() && seen.insert(t.to_string()) {
            out.push(t.to_string());
            if out.len() >= 12 {
                break;
            }
        }
    }
    out
}

/// Build a [`ClarifyQuestion`] from the brain reply, or `None` when it asked
/// nothing. A blank question yields `None` (no clarification needed).
fn build_clarify(brain: &BrainRoute) -> Option<ClarifyQuestion> {
    let q = brain.clarify_question.trim();
    if q.is_empty() {
        return None;
    }
    let options = brain
        .clarify_options
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    Some(ClarifyQuestion {
        question: q.to_string(),
        options,
    })
}

/// Map the brain's free-text `class` to a [`RouteClass`] (tolerant; `None` on an
/// unrecognised value so reconciliation keeps the floor's class).
fn parse_class(s: &str) -> Option<RouteClass> {
    match s
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
        "chat" | "conversation" | "smalltalk" | "small_talk" => Some(RouteClass::Chat),
        "explain" | "explanation" | "qa" | "question" | "answer" => Some(RouteClass::Explain),
        "quick_edit" | "quickedit" | "edit" | "tweak" | "small_change" => {
            Some(RouteClass::QuickEdit)
        }
        "debug" | "bugfix" | "fix" | "diagnose" => Some(RouteClass::Debug),
        "build" | "feature" | "product" | "greenfield" | "implement" => Some(RouteClass::Build),
        _ => None,
    }
}

/// Map the brain's `complexity` to a [`Depth`] (tolerant; `None` on unrecognised).
fn parse_depth(s: &str) -> Option<Depth> {
    match s.trim().to_ascii_lowercase().as_str() {
        "simple" | "trivial" | "small" | "fast" => Some(Depth::Fast),
        "medium" | "moderate" | "standard" => Some(Depth::Standard),
        "complex" | "hard" | "large" | "deep" => Some(Depth::Deep),
        _ => None,
    }
}

/// Map the brain's `kind` to a [`TaskKind`] (tolerant; `None` on unrecognised).
fn parse_kind(s: &str) -> Option<TaskKind> {
    match s
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-'], "_")
        .as_str()
    {
        "greenfield" | "new" | "product" => Some(TaskKind::Greenfield),
        "frontend_only" | "frontend" | "fe" | "ui" => Some(TaskKind::FrontendOnly),
        "backend_only" | "backend" | "be" | "api" => Some(TaskKind::BackendOnly),
        "bugfix" | "bug" | "fix" => Some(TaskKind::Bugfix),
        "refactor" => Some(TaskKind::Refactor),
        "docs_only" | "docs" | "documentation" | "research" => Some(TaskKind::DocsOnly),
        "light" | "small" | "trivial" => Some(TaskKind::Light),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use umadev_runtime::{
        CompletionRequest, CompletionResponse, Runtime, RuntimeError, RuntimeKind, Usage,
    };

    /// A one-shot brain that always returns the given triage JSON — exercises
    /// `route_via_brain` (the chat surface's brain-driven router).
    struct TriageBrain(&'static str);
    #[async_trait]
    impl Runtime for TriageBrain {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::Anthropic
        }
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, RuntimeError> {
            Ok(CompletionResponse {
                text: self.0.to_string(),
                id: "t".into(),
                model: "t".into(),
                usage: Usage::default(),
            })
        }
    }

    #[test]
    fn triage_prompt_sizes_a_document_as_docs_only_simple() {
        // The PRIMARY brain-first fix: the triage prompt must instruct the borrowed
        // brain to size a request to WRITE a document (the deliverable IS the document)
        // as `docs_only` / `simple` — distinct from building the product the document
        // describes. So the AUTHORITATIVE brain sizes a document light on the route
        // surface; the deterministic keyword tables are only the fail-open floor.
        let p = ROUTER_TRIAGE_SYSTEM;
        assert!(p.contains("docs_only"), "prompt names the docs_only kind");
        assert!(
            p.contains("WRITE / PRODUCE a DOCUMENT") || p.contains("WRITE a document"),
            "prompt distinguishes WRITING a document as the deliverable"
        );
        assert!(
            p.contains("complexity:simple") || p.contains("`complexity:simple`"),
            "a document is sized simple"
        );
        // It is framed as the OPPOSITE of building the product the document describes.
        assert!(
            p.to_lowercase().contains("opposite") && p.to_lowercase().contains("describes"),
            "the docs clause contrasts writing the spec vs. implementing it"
        );
    }

    #[tokio::test]
    async fn brain_sizes_a_document_write_light_no_team() {
        // End-to-end: when the brain returns `kind:docs_only, complexity:simple` for a
        // document write, the route is a light one — DocsOnly kind, Fast depth, and
        // ZERO team (a document does not convene a delivery roster).
        let brain = TriageBrain(
            "{\"class\":\"build\",\"kind\":\"docs_only\",\"complexity\":\"simple\",\"confidence\":0.9}",
        );
        let p = route_via_brain(&brain, "帮我写一份产品需求文档(PRD)").await;
        assert_eq!(p.kind, TaskKind::DocsOnly);
        assert_eq!(p.depth, Depth::Fast);
        assert!(p.team.is_empty(), "a document write convenes no team");
    }

    #[tokio::test]
    async fn brain_classifies_a_greeting_as_chat_not_build() {
        // The brain — not a keyword table — judges intent. A greeting is chat.
        let brain = TriageBrain(
            "{\"class\":\"chat\",\"kind\":\"light\",\"complexity\":\"simple\",\"confidence\":0.95}",
        );
        let p = route_via_brain(&brain, "你好,你是谁?能帮我做什么?").await;
        assert_eq!(p.class, RouteClass::Chat);
        assert!(p.team.is_empty());
    }

    #[tokio::test]
    async fn brain_classifies_a_real_build_as_build_with_team() {
        let brain = TriageBrain(
            "```json\n{\"class\":\"build\",\"kind\":\"greenfield\",\"complexity\":\"complex\",\
             \"needs\":[\"frontend\",\"backend\"],\"confidence\":0.9}\n```",
        );
        let p = route_via_brain(&brain, "做一个带登录的 SaaS 仪表盘").await;
        assert_eq!(p.class, RouteClass::Build);
        assert_eq!(p.depth, Depth::Deep);
        assert!(!p.team.is_empty(), "a complex build convenes a team");
    }

    #[tokio::test]
    async fn brain_build_with_unparseable_kind_still_convenes_a_team() {
        // MEDIUM #1: the brain says "build, complex" but garbles `kind` ("widget").
        // `parse_kind` fails → it must NOT fall back to `Light` (zero team). A
        // mutating class defaults to a build-shaped kind (Greenfield) so a deliberate
        // build always has a delivery roster.
        let brain = TriageBrain(
            "{\"class\":\"build\",\"kind\":\"widget\",\"complexity\":\"complex\",\"confidence\":0.9}",
        );
        let p = route_via_brain(&brain, "做一个完整的后台系统").await;
        assert_eq!(p.class, RouteClass::Build);
        assert_eq!(
            p.kind,
            TaskKind::Greenfield,
            "bad kind on a build → Greenfield"
        );
        assert!(
            !p.team.is_empty(),
            "a deliberate build with a bad kind must still convene a team"
        );
    }

    #[tokio::test]
    async fn brain_greenfield_narrows_to_backend_for_a_pure_backend_task() {
        // A weaker brain sizes a PURE backend task ("优化后端代码") as the broad greenfield;
        // the deterministic domain floor scopes the team to BackendOnly so it convenes no UI
        // reviewers (the reported "backend task pulls in a uiux-designer + frontend-engineer").
        let brain =
            TriageBrain("{\"class\":\"build\",\"kind\":\"greenfield\",\"complexity\":\"medium\"}");
        let p = route_via_brain(&brain, "优化后端代码,提升接口性能").await;
        assert_eq!(p.class, RouteClass::Build);
        assert_eq!(
            p.kind,
            TaskKind::BackendOnly,
            "a clearly backend build the brain called greenfield narrows to BackendOnly"
        );
        assert!(
            !p.team.contains(&Seat::UiuxDesigner) && !p.team.contains(&Seat::FrontendEngineer),
            "a pure-backend build convenes NO UI reviewers: {:?}",
            p.team
        );
    }

    #[tokio::test]
    async fn brain_greenfield_stays_greenfield_for_a_page_described_fullstack_build() {
        // HIGH #4: a full-stack app described purely by its PAGES ("博客系统,有文章列表和文章
        // 详情页面") has a frontend keyword (页面) and NO backend keyword, so the deterministic
        // classifier reads FrontendOnly. The brain authoritatively called it greenfield — the
        // domain floor must NOT narrow it to FrontendOnly and DROP the backend phase; a blog
        // needs persistence. It stays Greenfield with the full roster (incl. the backend seat).
        let brain =
            TriageBrain("{\"class\":\"build\",\"kind\":\"greenfield\",\"complexity\":\"complex\"}");
        let p = route_via_brain(&brain, "做一个博客系统,有文章列表和文章详情页面").await;
        assert_eq!(p.class, RouteClass::Build);
        assert_eq!(
            p.kind,
            TaskKind::Greenfield,
            "a page-described full-stack build must NOT be narrowed to frontend-only"
        );
        assert!(
            p.team.contains(&Seat::BackendEngineer),
            "the backend seat must survive: {:?}",
            p.team
        );
    }

    #[tokio::test]
    async fn brain_chat_with_unparseable_kind_keeps_light_no_team() {
        // The flip side: a read-only class (chat) with a bad kind keeps the light
        // `Light` default — no team is wanted on a chat turn regardless.
        let brain = TriageBrain(
            "{\"class\":\"chat\",\"kind\":\"widget\",\"complexity\":\"simple\",\"confidence\":0.9}",
        );
        let p = route_via_brain(&brain, "你好").await;
        assert_eq!(p.class, RouteClass::Chat);
        assert_eq!(p.kind, TaskKind::Light);
        assert!(p.team.is_empty());
    }

    #[tokio::test]
    async fn brain_prose_then_json_retry_recovers_a_build() {
        // LOW #1: the brain narrates intent on the FIRST reply (no JSON) — a real
        // build would otherwise degrade to Chat. The stricter JSON-only retry on the
        // second call recovers it. This brain returns prose first, JSON second.
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct ProseThenJson(AtomicUsize);
        #[async_trait]
        impl Runtime for ProseThenJson {
            fn kind(&self) -> RuntimeKind {
                RuntimeKind::Anthropic
            }
            async fn complete(
                &self,
                _req: CompletionRequest,
            ) -> Result<CompletionResponse, RuntimeError> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                let text = if n == 0 {
                    "Sure, this looks like a real build — I'd start by scaffolding the app."
                        .to_string()
                } else {
                    "{\"class\":\"build\",\"kind\":\"greenfield\",\"complexity\":\"complex\",\"confidence\":0.9}"
                        .to_string()
                };
                Ok(CompletionResponse {
                    text,
                    id: "t".into(),
                    model: "t".into(),
                    usage: Usage::default(),
                })
            }
        }
        let brain = ProseThenJson(AtomicUsize::new(0));
        let p = route_via_brain(&brain, "做一个完整的 SaaS 产品").await;
        assert_eq!(
            p.class,
            RouteClass::Build,
            "the JSON-only retry recovered the build"
        );
        assert!(!p.team.is_empty());
    }

    #[tokio::test]
    async fn brain_build_with_blank_complexity_floors_to_deliberate_with_ui_team() {
        // HIGH H1: a brain reply `{class:build, kind:frontend_only}` whose `complexity`
        // is blank/garbled must NOT degrade to a Fast build with an EMPTY team that
        // skips the plan+acceptance floor — the chat surface must get the SAME
        // treatment `/run` gives the same input (a UI review team + the deliberate
        // gate). The depth floors to at least Standard (deliberate) and the team is the
        // kind-sized UI roster.
        let brain = TriageBrain(
            "{\"class\":\"build\",\"kind\":\"frontend_only\",\"complexity\":\"\",\"confidence\":0.7}",
        );
        let p = route_via_brain(&brain, "做一个落地页").await;
        assert_eq!(p.class, RouteClass::Build);
        assert!(
            p.depth.is_deliberate(),
            "a product build with blank complexity floors to a deliberate depth, got {:?}",
            p.depth
        );
        assert!(
            !p.team.is_empty(),
            "a chat-surface UI build must convene a review team, not ship un-reviewed"
        );
        assert!(
            p.team.contains(&Seat::UiuxDesigner) && p.team.contains(&Seat::FrontendEngineer),
            "the team is the UI review roster: {:?}",
            p.team
        );
    }

    #[tokio::test]
    async fn brain_classifies_a_tweak_as_quick_edit() {
        let brain = TriageBrain(
            "{\"class\":\"quick_edit\",\"kind\":\"light\",\"complexity\":\"simple\",\"confidence\":0.8}",
        );
        let p = route_via_brain(&brain, "把标题改成 Welcome").await;
        assert_eq!(p.class, RouteClass::QuickEdit);
    }

    #[tokio::test]
    async fn brain_unavailable_degrades_to_chat_not_a_keyword_guess() {
        // Offline / unreachable brain → the lightest path (Chat), NOT a keyword
        // classifier. We depend on the base ecosystem; there is no deterministic
        // fallback classifier on this path by design.
        let offline = umadev_runtime::OfflineRuntime::new(RuntimeKind::Anthropic);
        let p = route_via_brain(&offline, "做一个待办应用").await;
        assert_eq!(p.class, RouteClass::Chat, "no brain → pass-through chat");
    }

    #[test]
    fn depth_turn_caps_are_ordered_generous_backstops() {
        // Item 1 tiers: deeper work earns more turns. The caps are a RUNAWAY BACKSTOP,
        // so each is comfortably above 1 (never a tight leash) and strictly ordered
        // Fast < Standard < Deep.
        assert!(Depth::Fast.max_turns() >= 1);
        assert!(
            Depth::Standard.max_turns() > Depth::Fast.max_turns(),
            "a deliberate build earns more turns than a chat/quick-edit"
        );
        assert!(
            Depth::Deep.max_turns() > Depth::Standard.max_turns(),
            "the deepest play earns the most turns"
        );
    }

    #[tokio::test]
    async fn a_deliberate_build_gets_a_higher_turn_cap_than_a_chat() {
        // The route's turn cap is derived from its depth: a real build (Standard/Deep)
        // sits well above a chat/quick-edit (Fast). Proven end-to-end off the routed
        // RoutePlan, not just the raw Depth mapping.
        let build = route(None, &opts(), "做一个待办事项 SaaS 产品").await;
        let chat = route(None, &opts(), "你好,在吗?").await;
        assert!(build.depth.is_deliberate());
        assert_eq!(chat.depth, Depth::Fast);
        assert!(
            build.max_turns() > chat.max_turns(),
            "a deliberate build ({}) must out-budget a chat turn ({})",
            build.max_turns(),
            chat.max_turns()
        );
    }

    fn opts() -> RunOptions {
        RunOptions {
            project_root: std::env::temp_dir(),
            requirement: String::new(),
            slug: "demo".to_string(),
            model: String::new(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: crate::trust::TrustMode::Guarded,
            strict_coverage: false,
        }
    }

    // ── Tier-0 deterministic classification ──

    #[tokio::test]
    async fn tier0_greeting_is_chat_no_session() {
        let p = route(None, &opts(), "你好,在吗?").await;
        assert_eq!(p.class, RouteClass::Chat);
        assert_eq!(p.depth, Depth::Fast);
        assert!(p.team.is_empty());
        assert!(p.needs_clarify.is_none());
    }

    #[tokio::test]
    async fn tier0_greenfield_is_deliberate_build() {
        let p = route(None, &opts(), "做一个待办事项 SaaS 产品").await;
        assert_eq!(p.class, RouteClass::Build);
        assert!(p.depth.is_deliberate());
        assert!(!p.team.is_empty(), "a real build convenes a team");
        assert!(p.class.mutates_workspace());
    }

    #[tokio::test]
    async fn tier0_quick_edit_is_fast_single_writer() {
        let p = route(None, &opts(), "改个文案,把标题改成 Welcome").await;
        // "改" is a work verb and the goal classifies Light/QuickEdit-ish → fast.
        assert_eq!(p.depth, Depth::Fast);
        assert!(matches!(p.class, RouteClass::QuickEdit | RouteClass::Debug));
        assert!(p.team.is_empty(), "a fast turn convenes no team");
    }

    #[tokio::test]
    async fn tier0_bugfix_is_debug() {
        let p = route(None, &opts(), "登录一直报错,帮我修一下").await;
        assert_eq!(p.class, RouteClass::Debug);
    }

    #[tokio::test]
    async fn small_create_request_is_a_build_not_a_quick_edit() {
        // "做一个待办单页" CREATES a new thing -> a (fast) Build that gets a visible
        // plan, NOT a QuickEdit. This is what the /run smoke mis-routed before.
        let p = route(
            None,
            &opts(),
            "做一个待办清单单页应用,纯前端,添加/完成/删除",
        )
        .await;
        assert_eq!(
            p.class,
            RouteClass::Build,
            "a create request must be a Build"
        );
    }

    #[tokio::test]
    async fn doc_request_is_a_light_quick_edit_with_no_team() {
        // The user-reported case: "generate a README" must NOT route to a heavyweight
        // build with a review team. A doc artifact is a quick file write — QuickEdit
        // (no plan synth, no team), and the lean QC short-circuit fires.
        for r in [
            "生成一个 README.md",
            "帮我写个 README 文件",
            "generate a README.md for this repo",
            "生成更新日志",
        ] {
            let p = route(None, &opts(), r).await;
            assert_eq!(p.depth, Depth::Fast, "a doc is fast: {r}");
            assert!(
                matches!(p.class, RouteClass::QuickEdit),
                "a doc artifact is a QuickEdit, not a Build: {r} (got {:?})",
                p.class
            );
            assert!(p.team.is_empty(), "a doc convenes NO review team: {r}");
        }
    }

    #[test]
    fn run_on_a_doc_forces_build_but_still_convenes_no_team() {
        // `/run` always forces a Build (the explicit-run contract), but the SIZING must
        // still scale a doc down: a Fast doc build ships no UI, so it convenes NO review
        // team — belt against a mis-classification exploding into a full review.
        for r in [
            "生成一个 README.md",
            "/run 生成 README",
            "write a CHANGELOG file",
        ] {
            let p = for_run(r);
            assert_eq!(p.class, RouteClass::Build, "/run forces Build: {r}");
            assert!(
                p.team.is_empty(),
                "a doc build convenes NO review team even under /run: {r} (team {:?})",
                p.team
            );
        }
    }

    #[test]
    fn ui_light_build_keeps_its_minimal_review_team() {
        // The guardrail must NOT regress: a genuine (small) UI page still earns the
        // minimal UI review core (designer + frontend + QA) — only non-UI docs/scripts
        // lose the team.
        let p = for_run("做一个简单的待办单页应用,纯前端,添加/删除");
        assert_eq!(p.class, RouteClass::Build);
        assert!(
            p.team.contains(&Seat::FrontendEngineer) && p.team.contains(&Seat::UiuxDesigner),
            "a UI page keeps the minimal UI review team (got {:?})",
            p.team
        );
    }

    #[test]
    fn genuine_full_build_still_convenes_the_full_team() {
        // The heavyweight path is INTACT: a real product build convenes the full
        // kind-sized roster (the review/quality machinery the task must not degrade).
        let p = for_run("做一个完整的电商网站,带账号、商品、购物车、支付和后台管理");
        assert_eq!(p.class, RouteClass::Build);
        assert!(
            p.depth.is_deliberate(),
            "a real product is a deliberate build"
        );
        assert!(
            p.team.len() >= 5,
            "a greenfield product convenes the full roster (got {:?})",
            p.team
        );
    }

    #[test]
    fn for_run_always_forces_build_even_for_a_terse_goal() {
        // The explicit /run command KNOWS the intent is a build — it must never
        // second-guess a clear/terse build into a quick-edit, so a plan always shows.
        // (A Fast single-page build legitimately convenes no critic team — only the
        // class is the invariant here.)
        for goal in ["做一个待办应用", "改个东西", "x", "a tiny thing"] {
            let p = for_run(goal);
            assert_eq!(p.class, RouteClass::Build, "/run forces Build for: {goal}");
        }
    }

    #[test]
    fn is_create_request_splits_create_from_edit() {
        assert!(is_create_request("做一个待办应用"));
        assert!(is_create_request("build me a landing page"));
        assert!(!is_create_request("改个文案,把标题改成 Welcome"));
        assert!(!is_create_request("rename this variable"));
    }

    #[tokio::test]
    async fn tier0_empty_requirement_is_chat() {
        let p = route(None, &opts(), "   ").await;
        assert_eq!(p.class, RouteClass::Chat);
    }

    // ── Budget + scope are deterministic ──

    #[test]
    fn budget_scales_with_class_and_depth() {
        let chat = Budget::for_route(RouteClass::Chat, Depth::Fast);
        let deep = Budget::for_route(RouteClass::Build, Depth::Deep);
        assert!(deep.max_tool_calls > chat.max_tool_calls);
        assert!(deep.max_tokens > chat.max_tokens);
    }

    #[test]
    fn scope_hints_extract_pathy_tokens() {
        let hints = path_hints_from_text("fix the bug in src/app.rs and styles.css");
        assert!(hints.iter().any(|h| h == "src/app.rs"));
        assert!(hints.iter().any(|h| h == "styles.css"));
    }

    // ── Reconciliation: brain escalates but never drops below the floor ──

    #[test]
    fn reconcile_brain_may_escalate_depth_and_team() {
        // Floor: a refactor → QuickEdit/Standard with no fast team.
        let floor = tier0("重构 auth 模块");
        let brain = BrainRoute {
            class: "build".to_string(),
            kind: "greenfield".to_string(),
            complexity: "complex".to_string(),
            confidence: 0.9,
            ..Default::default()
        };
        let out = reconcile(&floor, &brain, "重构 auth 模块");
        // Escalated to Build/Deep, team widened, never below the floor.
        assert_eq!(out.class, RouteClass::Build);
        assert_eq!(out.depth, Depth::Deep);
        assert!(out.confidence >= floor.confidence);
        assert!(out.team.len() >= floor.team.len());
    }

    #[test]
    fn reconcile_brain_cannot_drop_a_build_to_chat() {
        // Floor: a clear greenfield build.
        let floor = tier0("做一个完整的电商网站");
        assert_eq!(floor.class, RouteClass::Build);
        // The brain (wrongly) says "chat, simple". The floor must hold — a real
        // build can NEVER be silently de-scoped to chat.
        let brain = BrainRoute {
            class: "chat".to_string(),
            kind: "light".to_string(),
            complexity: "simple".to_string(),
            confidence: 0.95,
            ..Default::default()
        };
        let out = reconcile(&floor, &brain, "做一个完整的电商网站");
        assert_eq!(
            out.class,
            RouteClass::Build,
            "brain must not drop below floor"
        );
        assert!(out.depth.rank() >= floor.depth.rank());
        assert!(out.team.len() >= floor.team.len());
    }

    #[test]
    fn reconcile_honours_brain_clarification() {
        let floor = tier0("加个功能");
        let brain = BrainRoute {
            class: "build".to_string(),
            clarify_question: "前端还是后端功能?".to_string(),
            clarify_options: vec!["前端".to_string(), "后端".to_string()],
            ..Default::default()
        };
        let out = reconcile(&floor, &brain, "加个功能");
        let c = out.needs_clarify.expect("clarify present");
        assert_eq!(c.options.len(), 2);
        assert!(c.question.contains("前端"));
    }

    #[test]
    fn parse_helpers_are_tolerant() {
        assert_eq!(parse_class("Build"), Some(RouteClass::Build));
        assert_eq!(parse_class("quick-edit"), Some(RouteClass::QuickEdit));
        assert_eq!(parse_class("garbage"), None);
        assert_eq!(parse_depth("complex"), Some(Depth::Deep));
        assert_eq!(parse_depth("nope"), None);
        assert_eq!(parse_kind("frontend"), Some(TaskKind::FrontendOnly));
    }

    #[test]
    fn work_request_detector_is_bilingual() {
        assert!(looks_like_work_request("build me a login page"));
        assert!(looks_like_work_request("帮我做一个登录页"));
        assert!(!looks_like_work_request("你好啊"));
        assert!(!looks_like_work_request("nice, thanks"));
    }
}
