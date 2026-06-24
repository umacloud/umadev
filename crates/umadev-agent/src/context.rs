//! Firmware composition — UmaDev's identity + 心法 + JIT knowledge + pitfall
//! memory, assembled into ONE token-budgeted system prompt the host drivers
//! inject over the base's system-prompt surface.
//!
//! ## Why this exists (Wave 2, L0)
//!
//! UmaDev is smart hardware; the base CLI is the brain. Until Wave 2 the
//! default path injected only a static team-identity directive — the base
//! never received our team's accumulated knowledge or this project's recorded
//! pitfalls, so "the firmware that justifies the product" was barely plugged
//! in. [`compose_firmware`] is that firmware: it composes WHO you are (the
//! senior-team-director identity + the seat the current step needs), HOW this
//! team builds (the compact anti-AI-slop craft law), WHAT we've learned that
//! applies right now (a small JIT knowledge digest), and WHAT bit us before (a
//! small pitfall-memory digest) — then hands it to the base through the
//! continuous session's system-prompt face.
//!
//! ## The five layers (priority high → low)
//!
//! 1. **Identity** — always-on, short: the director + the role the route's
//!    work needs. [`crate::experts::agentic_team_identity`] + a route-derived
//!    seat persona.
//! 2. **心法 / anti-slop** — the team's craft law
//!    ([`crate::experts::agentic_engineering_rules`]); surfaced for work-class
//!    turns, skipped for pure chat.
//! 3. **Repo-map slice (JIT, brownfield-aware)** — a token-budgeted,
//!    scope-personalised signature outline of the user's OWN code via
//!    [`umadev_knowledge::repo_map`], so the base understands the existing
//!    codebase ("explain this code", "fix the bug in checkout", "add a field"
//!    all become repo-aware). Injected only when the project is non-empty (a
//!    greenfield/blank repo emits nothing — no scan, no tokens) and the turn
//!    is work-class (anything but pure chat). Higher priority than the curated
//!    knowledge digest: on a brownfield repo, the user's real structure is a
//!    sharper signal than a generic standard.
//! 4. **Pitfall memory (JIT)** — high-signal recorded pitfalls that match the
//!    project's tech-stack fingerprint + the requirement, via
//!    [`crate::lessons::relevant_lessons_for_prompt`] (a small digest, not the
//!    ledger). Work-class only.
//! 5. **Knowledge (JIT)** — the few most-relevant curated knowledge chunks for
//!    the requirement, via [`crate::phases::agentic_knowledge_digest`] (a small
//!    top-K, not the whole corpus). Work-class only.
//!
//! ## Token economy
//!
//! The whole prompt is bounded by [`FIRMWARE_BUDGET`]. Layers are appended in
//! the priority order above and the FIRST layer that would overflow is
//! truncated (head-kept) so the highest-priority material always survives:
//! identity beats 心法 beats memory beats knowledge. A chat turn injects only
//! the (short) identity — no retrieval — so day-to-day conversation stays fast.
//!
//! ## Fail-open by contract (mirrors the governance kernel + the router)
//!
//! Every retrieval is best-effort: a missing `knowledge/` dir, a disabled KB, an
//! empty index, no matching lesson, an empty/unreadable repo (no repo-map) — each
//! yields an empty layer, never an error. In the limit (everything fails) the
//! result is just the always-on identity, which is exactly the pre-Wave-2
//! behaviour. This function NEVER returns an error and NEVER blocks the base.

use std::path::Path;

use crate::experts::{
    agentic_engineering_rules, agentic_team_identity, excerpt, persona_for_role, ANTI_SLOP_LAW,
};
use crate::router::{RouteClass, RoutePlan};

/// The overall character ceiling for one composed firmware prompt.
///
/// Deliberately conservative (~10K chars ≈ a few K tokens): the firmware rides
/// on TOP of the base's own (large) default system prompt and the per-turn
/// directive, so it must stay a small, high-signal overlay — not a second
/// corpus. The host's `merge_prompt` has its own much larger backstop
/// (`MAX_SYSTEM = 90_000`) for the single-shot path; this is the tighter,
/// JIT-discipline budget for the firmware overlay specifically. The layers are
/// filled in priority order until this is hit (see [`compose_firmware`]).
pub const FIRMWARE_BUDGET: usize = 10_000;

/// The character budget the JIT tail (repo-map + pitfall memory + knowledge
/// digests) may add ON TOP of the always-on head (identity + 心法). Bounding the
/// tail keeps a single huge digest from ever dominating the prompt and crowding
/// the identity + craft law that MUST always lead a work turn. The always-on head
/// is pushed first and kept whole; only this tail is throttled.
///
/// This is sized to hold the repo-map slice ([`REPO_MAP_BUDGET`]) plus the
/// memory + knowledge digests together — so a brownfield turn carries its code
/// outline AND its learned/curated knowledge, while the head still always leads.
const ALWAYS_ON_RESERVE: usize = 6_800;

/// The character budget the brownfield repo-map slice (the signature outline of
/// the user's OWN code) may take inside the JIT tail. ~2.8K chars ≈ a compact
/// outline of the most-relevant files — enough to anchor the base in the real
/// codebase without the whole symbol graph crowding out the learned/curated
/// digests that share the tail. Greenfield repos contribute nothing (the slice
/// is empty), so this budget is spent only when there is real code to map.
const REPO_MAP_BUDGET: usize = 2_800;

/// How much firmware a route warrants — the JIT dial. Pure chat is the lightest
/// (identity only, no retrieval); a deliberate build is the fullest (every
/// layer). Derived deterministically from the route's class + depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirmwareTier {
    /// Identity only. Pure conversation / read-only explain — keep it light and
    /// fast, no knowledge/memory retrieval.
    Light,
    /// Identity + the compact craft law. A small, fast work turn (a quick edit /
    /// a shallow debug) — the visual + engineering moat, but no retrieval cost.
    Craft,
    /// Every layer: identity + craft + JIT knowledge + JIT pitfall memory. A real
    /// build / a deliberate turn, where the team's full experience earns its keep.
    Full,
}

impl FirmwareTier {
    /// Map a [`RoutePlan`] to its firmware tier. Chat / Explain are Light;
    /// a deliberate (Standard/Deep) turn or any Build is Full; everything else
    /// (a fast QuickEdit / a shallow Debug) is Craft.
    fn for_route(route: &RoutePlan) -> Self {
        match route.class {
            RouteClass::Chat | RouteClass::Explain => Self::Light,
            RouteClass::Build => Self::Full,
            // QuickEdit / Debug: full when the depth says deliberate, else just craft.
            RouteClass::QuickEdit | RouteClass::Debug => {
                if route.depth.is_deliberate() {
                    Self::Full
                } else {
                    Self::Craft
                }
            }
        }
    }

    /// Whether this tier surfaces the compact craft / anti-slop law (work turns).
    fn wants_craft(self) -> bool {
        matches!(self, Self::Craft | Self::Full)
    }

    /// Whether this tier runs the JIT knowledge + pitfall-memory retrieval.
    fn wants_jit(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Whether a route should carry the brownfield repo-map slice. Anything but pure
/// [`RouteClass::Chat`] benefits: an `Explain` ("explain this code") wants the
/// outline even though it injects no craft/knowledge; a `QuickEdit` / `Debug` /
/// `Build` all act on the existing code. Pure chat stays repo-map-free (fast +
/// no scan). The greenfield (empty-repo) skip is enforced separately by the slice
/// itself returning empty.
fn route_wants_repo_map(route: &RoutePlan) -> bool {
    route.class != RouteClass::Chat
}

/// Compose the firmware system prompt for ONE turn — the layered, budgeted,
/// route-tiered overlay the host injects over the base's system-prompt face.
///
/// `root` is the project root (where `knowledge/` + `.umadev/learned/` live);
/// `route` is Wave 1's typed [`RoutePlan`] for this turn (drives the tier + the
/// seat persona); `requirement` is the user's message (the retrieval query).
///
/// Returns the assembled prompt, always at least the always-on identity. The
/// layers are appended in priority order (identity → 心法 → memory → knowledge)
/// and truncated to [`FIRMWARE_BUDGET`], so the highest-priority material wins
/// the budget. **Fail-open:** any retrieval failure degrades that layer to empty;
/// in the limit the result is just the identity (the pre-Wave-2 behaviour). Never
/// errors, never blocks the base.
///
/// `async` so the caller can `.await` it inline at the (already-async) session
/// spawn / drive seam; the retrieval itself is synchronous + fail-open.
pub async fn compose_firmware(root: &Path, route: &RoutePlan, requirement: &str) -> String {
    let tier = FirmwareTier::for_route(route);
    let mut fw = FirmwareBuilder::new(FIRMWARE_BUDGET);

    // ── Layer 1: identity (always-on, highest priority) ──────────────────────
    // The director identity + the seat the route's work needs. Even a chat turn
    // carries the (short) identity so the base is always "us", never a bare CLI.
    fw.push_block(&identity_layer(route));

    // ── Layer 2: 心法 / anti-slop (work-class only) ──────────────────────────
    if tier.wants_craft() {
        fw.push_block(agentic_engineering_rules());
        // The full anti-slop / design-system law leads EVERY work turn, not just a
        // deliberate /run build: a chat-promoted build (the light resident-session
        // path) writes real UI too, and its visual quality is exactly the "moat" the
        // user judges. ANTI_SLOP_LAW is a STATIC string (no retrieval / no I/O), so
        // carrying it on the work-class head costs nothing on latency — the slow
        // layers are the JIT repo-map + knowledge below, which stay gated. Without
        // this, a UI built from chat skipped the design system and read as AI-slop.
        fw.push_block(ANTI_SLOP_LAW);
    }

    // The always-on head (identity + craft) is now fully in `buf` and can no longer
    // be evicted (later blocks only get truncated, never the ones already pushed).
    // Cap the JIT tail so the repo-map + memory + knowledge digests below add at most
    // ALWAYS_ON_RESERVE chars on top of the head — a giant digest can never dominate
    // the prompt, and the head always leads.
    fw.reserve_jit_tail(ALWAYS_ON_RESERVE);

    // ── Layer 3: repo-map slice (JIT, brownfield-aware) ──────────────────────
    // A scope-personalised signature outline of the user's OWN code, so the base
    // understands the existing codebase before it touches it. Pushed FIRST in the
    // JIT tail (ahead of memory + knowledge): on a brownfield repo, the user's real
    // structure is the sharper signal. Injected only when the route is work-class
    // (anything but pure chat) AND the repo is non-empty — a greenfield repo yields
    // an empty slice (no scan past the cached index, no tokens spent). The slice is
    // personalised by `route.scope` (the path hints the router surfaced) so the
    // files the turn is about rank first. Fail-open: empty/unreadable repo → skip.
    if route_wants_repo_map(route) {
        let repo_map = repo_map_layer(root, &route.scope);
        if !repo_map.trim().is_empty() {
            fw.push_block(&repo_map);
        }
    }

    // ── Layer 4: pitfall memory (JIT, work-class only) ───────────────────────
    // Recorded pitfalls matching this project's tech-stack fingerprint + the
    // requirement. Higher priority than fresh knowledge: "what bit us here" is a
    // sharper signal than "a relevant standard". Fail-open: empty digest → skip.
    if tier.wants_jit() {
        let memory = memory_layer(root, requirement);
        if !memory.trim().is_empty() {
            fw.push_block(&memory);
        }
    }

    // ── Layer 5: JIT knowledge (lowest priority, work-class only) ────────────
    // A small top-K of the most-relevant curated knowledge for the requirement —
    // a digest, never the whole corpus. Fail-open: empty digest → skip.
    if tier.wants_jit() {
        let knowledge = knowledge_layer(root, requirement);
        if !knowledge.trim().is_empty() {
            fw.push_block(&knowledge);
        }
    }

    fw.finish()
}

/// Build the identity layer: the always-on director identity plus, when the
/// route names a seat (the first of the convened team), that seat's persona — so
/// a frontend build opens "you are the director AND a senior frontend engineer".
/// Generalised (no external source); short by construction.
fn identity_layer(route: &RoutePlan) -> String {
    let mut out = String::from(agentic_team_identity());
    // The route's team is ordered doers-first; the lead seat names the craft the
    // current work needs. A chat/explain route has no team → no extra persona.
    if let Some(seat) = route.team.first() {
        let persona = persona_for_role(seat.role_id());
        if !persona.is_empty() {
            out.push_str("\n\n");
            out.push_str(persona);
        }
    }
    out
}

/// The pitfall-memory layer — the project's recorded pitfalls that match the
/// current tech-stack + requirement, via the SAME selector the runner uses
/// ([`crate::lessons::relevant_lessons_for_prompt`]). Reused (not re-derived) so
/// the firmware and the pipeline surface identical experience. Fail-open: a
/// project with no learned lessons returns an empty string.
fn memory_layer(root: &Path, requirement: &str) -> String {
    crate::lessons::relevant_lessons_for_prompt(root, requirement)
}

/// The JIT-knowledge layer — a small, requirement-scoped curated-knowledge digest
/// via the SAME compact retrieval the agentic path uses
/// ([`crate::phases::agentic_knowledge_digest`], capped at [`JIT_KNOWLEDGE_CHUNKS`]
/// short excerpts). Reused (not re-derived). Fail-open: no `knowledge/` dir, a
/// disabled KB, or no match → empty string.
fn knowledge_layer(root: &Path, requirement: &str) -> String {
    crate::phases::agentic_knowledge_digest(root, requirement, JIT_KNOWLEDGE_CHUNKS)
}

/// The brownfield repo-map layer — the [`project_context`] slice as a firmware
/// block. Thin wrapper so [`compose_firmware`] and any other path share ONE
/// auto-adopt primitive.
fn repo_map_layer(root: &Path, scope: &[String]) -> String {
    project_context(root, scope, REPO_MAP_BUDGET)
}

/// **Auto-adopt the project's code context** — a token-budgeted, `scope`-
/// personalised signature outline of the user's OWN repository, ready to inject
/// over the base's system-prompt face so the base understands the existing code
/// before it touches it. This is the brownfield-awareness primitive: it needs NO
/// manual `umadev adopt` step — the first call builds + mtime-caches the symbol
/// index ([`umadev_knowledge::symbol_index`]), and later calls are incremental
/// (re-scanning only changed files), so every path that conditions a base session
/// can be repo-aware for the cost of one cached scan.
///
/// `scope` is the router's path hints (substring-matched against file paths): the
/// files the turn is about rank first in the outline. `budget_chars` caps the
/// slice (typically [`REPO_MAP_BUDGET`]). The result is wrapped in a labelled
/// `# YOUR CODEBASE` block so the base reads it as the existing structure to
/// navigate/edit, not new code to write. Symbols are keyed `path:line`.
///
/// **Greenfield / fail-open:** an empty, blank, or unreadable repo yields an empty
/// `String` (no header, no tokens spent, no slowdown — the cached scan finds
/// nothing fast). This function never errors and never blocks the base. Shared by
/// [`compose_firmware`] (via [`repo_map_layer`]) and available to any other path
/// that wants the same outline.
#[must_use]
pub fn project_context(root: &Path, scope: &[String], budget_chars: usize) -> String {
    let outline = umadev_knowledge::repo_map(root, scope, budget_chars);
    if outline.trim().is_empty() {
        return String::new();
    }
    format!(
        "# YOUR CODEBASE — existing code structure (signature outline)\n\nThis is the \
         user's EXISTING repository. Read + edit these files; do NOT recreate what \
         already exists. Symbols are keyed `path:line`.\n\n{outline}"
    )
}

/// How many curated-knowledge chunks the firmware's JIT layer may carry — a small
/// top-K (a digest ≈ half a screen), never the whole corpus. Tighter than the
/// pipeline per-phase `top_k`: the firmware is an overlay, not the primary brief.
const JIT_KNOWLEDGE_CHUNKS: usize = 4;

/// A budget-bounded, priority-ordered prompt assembler. Blocks are pushed in
/// descending priority; once the running length would exceed the cap the next
/// block is head-truncated (or dropped if there's no room left), so the
/// highest-priority blocks always survive. A later [`reserve_jit_tail`] caps how
/// much the lower-priority JIT layers may add on top of the always-on head.
///
/// [`reserve_jit_tail`]: FirmwareBuilder::reserve_jit_tail
struct FirmwareBuilder {
    buf: String,
    cap: usize,
}

impl FirmwareBuilder {
    fn new(cap: usize) -> Self {
        Self {
            buf: String::new(),
            cap,
        }
    }

    /// Cap the budget the JIT tail (every block pushed AFTER this call) may use,
    /// to at most `tail` characters on top of the already-assembled always-on
    /// head. Concretely: lower the cap to `min(cap, used + tail)` (never raise it),
    /// so the head is kept whole and the JIT layers share only the smaller tail
    /// budget — a giant lesson/knowledge digest can never dominate the prompt.
    fn reserve_jit_tail(&mut self, tail: usize) {
        let used = self.buf.chars().count();
        self.cap = self.cap.min(used + tail);
    }

    /// Append one block (separated by a blank line), head-truncating it to fit the
    /// remaining budget. A block with no room left is dropped entirely. Empty input
    /// is a no-op.
    fn push_block(&mut self, block: &str) {
        let block = block.trim();
        if block.is_empty() {
            return;
        }
        let used = self.buf.chars().count();
        let sep = if self.buf.is_empty() { 0 } else { 2 }; // "\n\n"
        let remaining = self.cap.saturating_sub(used + sep);
        if remaining == 0 {
            return; // no room — drop this (lower-priority) block
        }
        if !self.buf.is_empty() {
            self.buf.push_str("\n\n");
        }
        if block.chars().count() <= remaining {
            self.buf.push_str(block);
        } else {
            // Head-keep the part that fits — a truncated high-value block still
            // beats dropping it (mirrors `experts::excerpt`).
            self.buf.push_str(&excerpt(block, remaining));
        }
    }

    fn finish(self) -> String {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::planner::TaskKind;
    use crate::router::{Budget, Depth};

    /// A minimal [`RoutePlan`] for a given class/depth/team, so the tests drive
    /// `compose_firmware` without a live router/session.
    fn route(class: RouteClass, depth: Depth, team: Vec<Seat>) -> RoutePlan {
        RoutePlan {
            class,
            kind: TaskKind::Greenfield,
            depth,
            team,
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(class, depth),
            confidence: 0.6,
        }
    }

    #[tokio::test]
    async fn chat_route_injects_only_the_light_identity() {
        // A pure chat turn must stay light: the (short) identity, NO craft law,
        // NO knowledge/memory retrieval block.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(RouteClass::Chat, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "你好,在吗?").await;
        assert!(fw.to_lowercase().contains("umadev"), "carries identity");
        assert!(fw.to_lowercase().contains("director"));
        // The compact craft block + the anti-slop law are work-class only.
        assert!(
            !fw.contains("HOW YOUR TEAM BUILDS"),
            "chat must not carry the craft law: {fw}"
        );
        assert!(
            !fw.contains("ANTI-AI-SLOP"),
            "chat must not carry anti-slop"
        );
        // And it stays small (well under the budget).
        assert!(fw.chars().count() < ALWAYS_ON_RESERVE);
    }

    #[tokio::test]
    async fn build_route_layers_identity_craft_and_anti_slop() {
        // A real build is the FULL tier: identity + the compact craft block + the
        // full anti-slop law (its visual moat is load-bearing on a build).
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个待办事项 SaaS 产品").await;
        assert!(fw.to_lowercase().contains("umadev"));
        assert!(fw.contains("HOW YOUR TEAM BUILDS"), "craft law present");
        assert!(fw.contains("ANTI-AI-SLOP"), "anti-slop present on a build");
    }

    #[tokio::test]
    async fn build_route_opens_the_lead_seat_persona() {
        // The lead seat in the route's team contributes its persona, so a frontend
        // build opens "you are ... a senior frontend engineer".
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个登录页").await;
        assert!(
            fw.to_lowercase().contains("frontend engineer"),
            "lead seat persona injected: {fw}"
        );
    }

    #[tokio::test]
    async fn quick_edit_carries_the_full_design_system_but_no_slow_retrieval() {
        // The design-system / anti-slop law is ALWAYS-ON for any work turn (a
        // quick-edit or a chat-promoted build writes real UI too — its visual
        // quality is the moat the user judges). It's a STATIC string, so it costs
        // nothing on latency. What stays gated is the SLOW JIT retrieval (repo-map /
        // knowledge / memory) — those do real I/O, so a fast turn skips them and the
        // base reads what it needs via its own tools.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(RouteClass::QuickEdit, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "改个文案").await;
        assert!(
            fw.contains("HOW YOUR TEAM BUILDS"),
            "craft present on a work turn"
        );
        assert!(
            fw.contains("ANTI-AI-SLOP"),
            "the design-system law is always-on for a work turn (every UI must be exquisite)"
        );
        // …but the SLOW JIT retrieval (knowledge / memory) stays gated for speed.
        assert!(!fw.contains("Lessons from prior runs"));
        assert!(!fw.contains("YOUR TEAM'S EXPERIENCE"));
    }

    #[tokio::test]
    async fn knowledge_layer_is_injected_when_corpus_matches() {
        // With a matching curated-knowledge file present, the Full tier surfaces a
        // small knowledge digest (the JIT layer). Fail-open is covered separately.
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/security");
        std::fs::create_dir_all(&kd).unwrap();
        std::fs::write(
            kd.join("login.md"),
            "# Login\n\n## OAuth\n\nUse OAuth2 with PKCE for login authentication and token rotation.",
        )
        .unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "login oauth authentication").await;
        assert!(
            fw.contains("YOUR TEAM'S EXPERIENCE"),
            "knowledge digest header present when the corpus matches: {fw}"
        );
        assert!(
            fw.contains("login"),
            "the matched chunk path/body is surfaced"
        );
    }

    #[tokio::test]
    async fn fail_open_when_no_knowledge_and_no_lessons() {
        // A bare project (no knowledge/ dir, no learned lessons) must still produce
        // a valid firmware — just the always-on layers, never an error/empty.
        // Neutralise the bundled-corpus fallbacks so this holds even on a machine
        // that has staged ~/.umadev/knowledge via a real binary run.
        let _no_corpus = crate::test_support::NoBundledCorpus::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(RouteClass::Build, Depth::Deep, vec![Seat::FrontendEngineer]);
        let fw = compose_firmware(tmp.path(), &r, "build something").await;
        assert!(!fw.is_empty());
        assert!(fw.to_lowercase().contains("umadev"), "identity survives");
        assert!(fw.contains("HOW YOUR TEAM BUILDS"), "craft survives");
        // No retrieval blocks (nothing on disk to retrieve).
        assert!(!fw.contains("Lessons from prior runs"));
        assert!(!fw.contains("YOUR TEAM'S EXPERIENCE"));
    }

    /// Seed a small but real source tree so [`umadev_knowledge::repo_map`] finds
    /// symbols (a non-empty / brownfield repo). Uses distinct exported symbols so
    /// the signature outline is non-trivial.
    fn seed_brownfield(root: &std::path::Path) {
        std::fs::write(
            root.join("checkout.ts"),
            "export function computeCartTotal(items) { return 0; }\n\
             export class CheckoutService { pay() {} }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("auth.ts"),
            "export function loginUser(email) { return true; }\n\
             export function logoutUser() {}\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn brownfield_repo_injects_the_repo_map_slice() {
        // A work-class turn on a NON-EMPTY repo carries the repo-map slice so the
        // base understands the existing code before it edits it.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_brownfield(tmp.path());
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "在结算流程里修一个 bug").await;
        assert!(
            fw.contains("YOUR CODEBASE"),
            "brownfield firmware must carry the repo-map slice header: {fw}"
        );
        // The outline names real symbols/files from the seeded tree.
        assert!(
            fw.contains("checkout.ts") || fw.contains("computeCartTotal"),
            "repo-map names real code from the repo: {fw}"
        );
    }

    #[tokio::test]
    async fn greenfield_repo_injects_no_repo_map_slice() {
        // A blank/greenfield repo (no source files) must NOT carry a repo-map slice
        // — no header, no wasted tokens, no slowdown over the pre-Wave-3 firmware.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个全新的待办事项产品").await;
        assert!(
            !fw.contains("YOUR CODEBASE"),
            "greenfield firmware must NOT carry a repo-map slice: {fw}"
        );
    }

    #[tokio::test]
    async fn pure_chat_skips_the_repo_map_even_on_a_brownfield_repo() {
        // Pure chat stays light: even with real code on disk, a chat turn carries no
        // repo-map (no scan, fast day-to-day conversation).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_brownfield(tmp.path());
        let r = route(RouteClass::Chat, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "你好,在吗?").await;
        assert!(
            !fw.contains("YOUR CODEBASE"),
            "chat must not carry the repo-map slice: {fw}"
        );
    }

    #[tokio::test]
    async fn explain_on_a_brownfield_repo_gets_repo_map_even_though_light_tier() {
        // "explain this code" routes to Explain (Light tier — no craft/knowledge) but
        // STILL needs the repo-map: understanding the existing code is the whole task.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_brownfield(tmp.path());
        let r = route(RouteClass::Explain, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "解释一下这段代码是做什么的").await;
        assert!(
            fw.contains("YOUR CODEBASE"),
            "explain on a brownfield repo carries the repo-map slice: {fw}"
        );
        // Light tier still holds: no craft law / anti-slop on an explain turn.
        assert!(!fw.contains("HOW YOUR TEAM BUILDS"), "explain stays Light");
    }

    #[tokio::test]
    async fn repo_map_scope_personalises_file_order() {
        // The router's `scope` hints rank matching files first in the slice — a turn
        // about checkout surfaces checkout.ts ahead of auth.ts.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_brownfield(tmp.path());
        let mut r = route(
            RouteClass::Debug,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        r.scope = vec!["checkout".to_string()];
        let fw = compose_firmware(tmp.path(), &r, "结算有问题").await;
        let map_start = fw.find("YOUR CODEBASE").expect("repo-map present");
        let slice = &fw[map_start..];
        let checkout_at = slice.find("checkout.ts");
        let auth_at = slice.find("auth.ts");
        // checkout must be present and, when both appear, ordered before auth.
        assert!(checkout_at.is_some(), "scoped file present: {slice}");
        if let (Some(c), Some(a)) = (checkout_at, auth_at) {
            assert!(
                c < a,
                "scope hint ranks checkout.ts before auth.ts: {slice}"
            );
        }
    }

    #[tokio::test]
    async fn repo_map_layer_is_fail_open_on_an_unreadable_root() {
        // A root that doesn't exist (or can't be scanned) yields an empty slice — the
        // firmware degrades to the head-only behaviour, never an error.
        let missing = std::path::Path::new("/nonexistent/umadev/repo/path/xyz");
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(missing, &r, "build something").await;
        assert!(!fw.is_empty(), "firmware still composed");
        assert!(
            !fw.contains("YOUR CODEBASE"),
            "no repo-map from an unreadable root"
        );
    }

    #[test]
    fn project_context_greenfield_is_empty() {
        // Auto-adopt on a blank repo yields nothing — no header, no tokens.
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = project_context(tmp.path(), &[], REPO_MAP_BUDGET);
        assert!(ctx.is_empty(), "greenfield project_context is empty: {ctx}");
    }

    #[test]
    fn project_context_brownfield_yields_a_labelled_outline() {
        // Auto-adopt on a real repo yields the labelled # YOUR CODEBASE outline,
        // naming real symbols — and needs NO manual adopt marker.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_brownfield(tmp.path());
        let ctx = project_context(tmp.path(), &[], REPO_MAP_BUDGET);
        assert!(ctx.contains("YOUR CODEBASE"), "labelled block: {ctx}");
        assert!(
            ctx.contains("checkout.ts") || ctx.contains("auth.ts"),
            "names real files: {ctx}"
        );
        assert!(
            ctx.chars().count() <= REPO_MAP_BUDGET + 400,
            "respects budget"
        );
    }

    #[test]
    fn project_context_is_stable_across_repeated_calls_incremental_cache() {
        // The second call reuses the mtime-cached symbol index (no rescan needed) and
        // returns the same outline — the incremental auto-adopt contract.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_brownfield(tmp.path());
        let first = project_context(tmp.path(), &[], REPO_MAP_BUDGET);
        let second = project_context(tmp.path(), &[], REPO_MAP_BUDGET);
        assert_eq!(first, second, "cached re-derivation is stable");
        assert!(!first.is_empty());
    }

    #[tokio::test]
    async fn never_exceeds_the_budget() {
        // Even with a huge matching corpus the composed firmware respects the cap.
        let tmp = tempfile::TempDir::new().unwrap();
        let kd = tmp.path().join("knowledge/frontend");
        std::fs::create_dir_all(&kd).unwrap();
        // A big doc with many sections that all match the query.
        let mut big = String::from("# Frontend Standards\n");
        for i in 0..200 {
            big.push_str(&format!(
                "\n## Section {i} login design tokens\n\nlogin design tokens components states \
                 accessibility responsive layout {}\n",
                "x".repeat(300)
            ));
        }
        std::fs::write(kd.join("standards.md"), &big).unwrap();
        let r = route(RouteClass::Build, Depth::Deep, vec![Seat::FrontendEngineer]);
        let fw = compose_firmware(tmp.path(), &r, "login design tokens components").await;
        assert!(
            fw.chars().count() <= FIRMWARE_BUDGET,
            "firmware must stay within the budget ({} > {FIRMWARE_BUDGET})",
            fw.chars().count()
        );
    }

    #[tokio::test]
    async fn jit_tail_is_bounded_so_a_giant_digest_cannot_dominate() {
        // The reserve caps the memory+knowledge tail to ALWAYS_ON_RESERVE chars on
        // top of the always-on head: a huge matching corpus must add at most that
        // much over the head-only (no-knowledge) firmware. Locks the priority floor.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(RouteClass::Build, Depth::Deep, vec![Seat::FrontendEngineer]);
        // Head-only firmware: identity + craft + anti-slop, no corpus on disk.
        let head_only = compose_firmware(tmp.path(), &r, "login design tokens").await;
        let head_len = head_only.chars().count();
        // Now seed a huge matching corpus and recompose.
        let kd = tmp.path().join("knowledge/frontend");
        std::fs::create_dir_all(&kd).unwrap();
        let mut big = String::from("# Frontend Standards\n");
        for i in 0..200 {
            big.push_str(&format!(
                "\n## Section {i} login design tokens\n\nlogin design tokens components states {}\n",
                "x".repeat(300)
            ));
        }
        std::fs::write(kd.join("standards.md"), &big).unwrap();
        let with_jit = compose_firmware(tmp.path(), &r, "login design tokens components").await;
        assert!(
            with_jit.chars().count() <= head_len + ALWAYS_ON_RESERVE,
            "JIT tail must be bounded by the reserve ({} > {head_len} + {ALWAYS_ON_RESERVE})",
            with_jit.chars().count()
        );
    }

    #[test]
    fn budget_keeps_highest_priority_block_when_overflowing() {
        // The builder fills in priority order and head-truncates; the FIRST (highest
        // priority) block must always be present, a later one may be dropped.
        let mut b = FirmwareBuilder::new(50);
        b.push_block("IDENTITY-BLOCK-HEAD"); // 19 chars — fits
        b.push_block(&"L".repeat(100)); // overflow — truncated/partial
        let out = b.finish();
        assert!(out.contains("IDENTITY-BLOCK-HEAD"), "head block kept whole");
        assert!(out.chars().count() <= 50, "respects the cap");
    }

    #[test]
    fn builder_drops_a_block_with_no_room_left() {
        let mut b = FirmwareBuilder::new(20);
        b.push_block(&"A".repeat(20)); // fills the budget exactly
        b.push_block("THIS-SHOULD-BE-DROPPED");
        let out = b.finish();
        assert!(!out.contains("DROPPED"), "no-room block is dropped");
        assert!(out.chars().count() <= 20);
    }

    #[test]
    fn tier_mapping_matches_route_class_and_depth() {
        let chat = route(RouteClass::Chat, Depth::Fast, Vec::new());
        assert_eq!(FirmwareTier::for_route(&chat), FirmwareTier::Light);
        let explain = route(RouteClass::Explain, Depth::Fast, Vec::new());
        assert_eq!(FirmwareTier::for_route(&explain), FirmwareTier::Light);
        let qe = route(RouteClass::QuickEdit, Depth::Fast, Vec::new());
        assert_eq!(FirmwareTier::for_route(&qe), FirmwareTier::Craft);
        let dbg_deep = route(RouteClass::Debug, Depth::Deep, Vec::new());
        assert_eq!(FirmwareTier::for_route(&dbg_deep), FirmwareTier::Full);
        let build = route(RouteClass::Build, Depth::Fast, Vec::new());
        assert_eq!(FirmwareTier::for_route(&build), FirmwareTier::Full);
    }
}
