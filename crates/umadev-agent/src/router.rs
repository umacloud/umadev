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
            RouteClass::Build => format!(
                "这是一次完整构建({}),进研发流程:计划 + 团队 + 质量门。",
                self.depth.as_str()
            ),
        }
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
        // Fast, and ensure a real build team rather than a chat one.
        if matches!(r.depth, Depth::Fast) && r.team.is_empty() {
            // keep Fast (proportional) but give it a build team
        }
        r.team = tier0_team(r.kind, RouteClass::Build, r.depth);
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
    let team = tier0_team(kind, class, depth);
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
            if is_create_request(requirement) {
                (RouteClass::Build, Depth::Fast)
            } else {
                (RouteClass::QuickEdit, Depth::Fast)
            }
        }
    }
}

/// Tier-0 team for a (kind, class, depth). Reuses the planner's complexity sense:
/// a fast/light turn convenes NO team; a standard/deep build convenes the seats the
/// kind needs. Deterministic; the brain may widen it during reconciliation.
fn tier0_team(kind: TaskKind, class: RouteClass, depth: Depth) -> Vec<Seat> {
    // Pure conversation / read-only explanation → no team (it's overhead there).
    if matches!(class, RouteClass::Chat | RouteClass::Explain) {
        return Vec::new();
    }
    // A real BUILD ALWAYS convenes a team — even a Fast one. A chat-built page is
    // still a delivery and its UI/quality must be reviewed (the audit caught a Fast
    // build convening ZERO critics → a landing page shipped un-reviewed). A Fast
    // build gets a MINIMAL review team (designer + frontend + QA, the UI-quality
    // core); a deliberate build gets the full kind-sized roster.
    if matches!(class, RouteClass::Build) {
        if depth == Depth::Fast {
            return vec![Seat::UiuxDesigner, Seat::FrontendEngineer, Seat::QaEngineer];
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
    #[serde(default)]
    needs: Vec<String>,
    /// Likely-relevant files / dirs.
    #[serde(default)]
    scope: Vec<String>,
    // NB: the prompt also invites a `risks` array; the router doesn't surface risks
    // (that's the plan's job — see `plan_state`), so it's intentionally not a field
    // here. serde ignores the unknown key, keeping the brain's schema unchanged.
    /// A clarifying question, when the request is genuinely ambiguous.
    #[serde(default)]
    clarify_question: String,
    /// Discrete options for the clarifying question.
    #[serde(default)]
    clarify_options: Vec<String>,
    /// The brain's confidence `0.0..=1.0` (tolerant: out-of-range is clamped).
    #[serde(default)]
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
        Some(brain) => brain_to_route(&brain),
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
fn brain_to_route(brain: &BrainRoute) -> RoutePlan {
    let class = parse_class(&brain.class).unwrap_or(RouteClass::Chat);
    let depth = parse_depth(&brain.complexity).unwrap_or(Depth::Fast);
    // Default kind: a mutating class (build / quick-edit / debug) whose `kind` field
    // is unparseable must NOT fall back to `Light` — `Light` convenes ZERO team, so a
    // brain that says "build, complex" but garbles `kind` would silently lose the
    // delivery roster (a deliberate build with no critics). Default a mutating class
    // to a BUILD-SHAPED kind (`Greenfield` → the full roster via `team_for_kind`); a
    // read-only class (chat / explain) keeps the light `Light` default (no team
    // wanted there anyway). The brain may still narrow it via a parseable `kind`.
    let kind = parse_kind(&brain.kind).unwrap_or_else(|| {
        if class.mutates_workspace() {
            TaskKind::Greenfield
        } else {
            TaskKind::Light
        }
    });
    let team = reconcile_team(&[], kind, class, depth, &brain.needs);
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
