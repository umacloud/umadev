//! Intelligent intent router (Wave 1, L1) — UmaDev's "thinking" primitive.
//!
//! The router is UmaDev borrowing the base's brain to DECIDE *how* to handle a
//! turn, before any work begins. It produces one typed [`RoutePlan`] the caller
//! reads to choose a path (fast / deliberate / clarify), size the team, and budget
//! the turn. It performs NO work itself and owns NO model — it consults the borrowed
//! brain over a read-only fork, exactly like the proven critic / intake patterns.
//!
//! ## Two tiers, both fail-open
//!
//! - **Tier-0 (deterministic, zero-latency, the FLOOR and the FALLBACK):** the
//!   existing [`crate::planner::classify`] keyword table + a bilingual work-request
//!   detector ([`looks_like_work_request`]) catch the obvious — a greeting routes to
//!   [`RouteClass::Chat`], a "改个文案" routes to [`RouteClass::QuickEdit`]. This
//!   ALWAYS runs and ALWAYS produces a complete, safe [`RoutePlan`].
//! - **Tier-1 (brain-assisted, optional):** for an ambiguous request, one strict-JSON
//!   consult on a `fork()`ed read-only session (cloned from the critic team's
//!   [`crate::continuous::ForkConsult`] mechanism) returns
//!   `{class, kind, complexity, needs, scope, risks, confidence}`. It is reconciled
//!   with the Tier-0 prior under one rule: **the brain may ESCALATE (raise depth,
//!   widen the team) but may never DROP BELOW the deterministic safe floor** — it
//!   cannot silently de-scope a request the keywords flagged as real work.
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
        // A trivially-small build → QuickEdit (fast single-writer).
        TaskKind::Light => (RouteClass::QuickEdit, Depth::Fast),
    }
}

/// Tier-0 team for a (kind, class, depth). Reuses the planner's complexity sense:
/// a fast/light turn convenes NO team; a standard/deep build convenes the seats the
/// kind needs. Deterministic; the brain may widen it during reconciliation.
fn tier0_team(kind: TaskKind, class: RouteClass, depth: Depth) -> Vec<Seat> {
    // Chat / Explain / any Fast turn → no team (the team is overhead there).
    if matches!(class, RouteClass::Chat | RouteClass::Explain) || depth == Depth::Fast {
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
async fn consult_route(
    session: &mut dyn BaseSession,
    _options: &RunOptions,
    requirement: &str,
) -> Option<BrainRoute> {
    let system = "You are a senior engineering director triaging ONE incoming request before \
         any work starts. Decide how to handle it. Be decisive and terse. \
         `class`: chat (small talk) | explain (read-only Q&A about code) | quick_edit (a \
         small, well-scoped change) | debug (diagnose+fix a defect) | build (a real \
         feature/product). `kind`: greenfield | frontend_only | backend_only | bugfix | \
         refactor | docs_only | light. `complexity`: simple | medium | complex. Only set \
         `clarify_question` when the request is genuinely ambiguous in a way you could NOT \
         resolve by reading the code — never ask what you can discover yourself. JSON shape: \
         {\"class\":\"…\",\"kind\":\"…\",\"complexity\":\"simple|medium|complex\",\
         \"needs\":[\"…\"],\"scope\":[\"file/dir\",…],\"risks\":[\"…\"],\
         \"clarify_question\":\"\",\"clarify_options\":[],\"confidence\":0.0}";
    let user = format!("Request:\n{requirement}");

    // Fork a read-only session (bounded handshake) and run one strict-JSON judge
    // turn over it — reusing the exact ForkConsult mechanism the critic team uses.
    let fork = crate::continuous::fork_with_timeout(session).await;
    let consult = crate::continuous::ForkConsult::new(fork);
    let json_text = consult.judge_json("router", system, user).await;
    consult.end().await;

    let text = json_text?;
    serde_json::from_str::<BrainRoute>(&text).ok()
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
