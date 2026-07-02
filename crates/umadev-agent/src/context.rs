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
//! ## KV-cache-stable prefix (base-I/O economy)
//!
//! The layer order is ALSO chosen for the base's prompt KV-cache. The maximally
//! STABLE material — identity → output-language → craft law → anti-slop law →
//! user charter — is emitted FIRST and is byte-identical across turns that differ
//! only in their per-turn inputs, so the base re-uses its cached attention over
//! that prefix instead of re-paying the whole prompt every turn. Everything that
//! changes turn to turn — the recorded project facts, the app-runtime directive
//! (keyed off the requirement), the repo-map slice, the pitfall digest, the
//! knowledge digest — is emitted AFTER that stable prefix, and each such block is
//! deterministically ordered (no `HashMap` iteration, no timestamp high in the
//! prefix). Reordering a volatile block above the stable head would silently bust
//! the cache and re-pay the prefix every turn; the `stable_prefix_*` lock tests
//! pin the boundary so a future edit can't regress it.
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

/// The character budget the user's EDITED team charter
/// ([`crate::constitution::user_charter_firmware_block`]) may take in the
/// always-on work-class head. The firmware already injects the built-in craft +
/// anti-slop law, so this is spent ONLY when the user has actually customized
/// `.umadev/constitution.md` (a pristine default injects nothing) — making the
/// user's own non-negotiables operative without duplicating the built-ins.
const CONSTITUTION_BUDGET: usize = 1_400;

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

    // ── Always-on: OUTPUT LANGUAGE ───────────────────────────────────────────
    // The base must reply in the user's interface language (the i18n locale), not
    // default to English. User-reported: a zh-CN user saw English replies like
    // "This is a monorepo. Let me map it out in depth…". Pushed right after the
    // identity so it leads every turn (chat AND build); empty for an English locale
    // (the base's default — no tokens spent).
    let lang_directive = language_directive();
    if !lang_directive.is_empty() {
        fw.push_block(&lang_directive);
    }

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

        // ── The team's CHARTER (only when the user has EDITED it) ────────────
        // The constitution (`.umadev/constitution.md`) makes the firmware's
        // non-negotiables a thing the user can read AND edit. When the user has
        // customized it, surface their version so their own operating principles
        // reach the base on work turns — the built-in craft law above already
        // covers a pristine/absent charter, so this injects NOTHING in the common
        // case (no duplication, no extra tokens). One small synchronous read of a
        // tiny file; fail-open (no file / unreadable → empty). Part of the
        // always-on head (pushed before `reserve_jit_tail`), so a user-edited
        // charter is high-priority, not throttled with the JIT tail.
        let charter = crate::constitution::user_charter_firmware_block(root, CONSTITUTION_BUDGET);
        if !charter.trim().is_empty() {
            fw.push_block(&charter);
        }

        // ── OPEN-DECISIONS discipline — the parking-lot DIRECTIVE (static) ────
        // The third durable-memory channel (sibling of the recorded facts below +
        // the pitfall ledger): a committed register of items left undecided /
        // deferred / blocked / parked pending a future trigger, so a real dev
        // team's parking-lot list is built into EVERY project instead of relying
        // on the base to hold open items in working memory (where they are lost).
        // This block is the RECORD guidance — WHEN/WHERE to append, the append-
        // only + resolved-in-place discipline, the three categories + the entry
        // fields. It is a byte-STATIC `&'static str` (no per-turn data), so it
        // sits in the KV-cache-stable head like the anti-slop law — a fixed
        // policy, paid once. The volatile RECALL of the actual unresolved items
        // is injected below the boundary (next to the recorded facts). Always-on
        // for work turns ON PURPOSE: without it a fresh project would never be
        // told to record its FIRST open item.
        fw.push_block(crate::open_decisions::decisions_directive());

        // ══ STABLE → VOLATILE BOUNDARY (KV-cache) ════════════════════════════
        // Everything ABOVE this point (identity, output-language, craft law,
        // anti-slop law, user charter) is byte-stable across turns and forms the
        // base's cacheable prefix. Everything BELOW — recorded facts, the
        // requirement-keyed app-runtime directive, and the JIT tail (repo-map,
        // pitfall + knowledge digests) — changes turn to turn and is placed AFTER
        // the stable head ON PURPOSE so the prefix keeps hitting the KV-cache. Do
        // NOT move a volatile block above this line (it busts the cache); see the
        // `stable_prefix_*` lock tests.

        // ── Durable PROJECT FACTS — recalled on EVERY work turn ──────────────
        // Facts the team already resolved about THIS project (a JDK/binary path, a
        // required version/port, a build/run/test command, an architecture decision,
        // a user preference), persisted to `.umadev/memory/facts.jsonl`. Recalled
        // into the ALWAYS-ON head (not the throttled JIT tail) ON PURPOSE: the whole
        // point is the base sees the facts regardless of the bounded transcript or a
        // base context rotation, so it never re-searches a fact it already found —
        // head placement guarantees they survive the budget. The block also carries
        // the record-guidance, so the base persists new durable facts the same way.
        // Bounded ([`crate::project_facts::FACTS_FIRMWARE_BUDGET`]) + fail-open: no
        // store / a corrupt store → empty, behaving exactly as before. One small
        // inline read, like the charter read above; nothing on a pure-chat turn.
        let facts = crate::project_facts::facts_firmware_block(
            root,
            crate::project_facts::FACTS_FIRMWARE_BUDGET,
        );
        if !facts.trim().is_empty() {
            fw.push_block(&facts);
        }

        // ── OPEN-DECISIONS RECALL — unresolved items resurface (volatile) ────
        // The still-UNRESOLVED parking-lot items for THIS project, prefixed with
        // the `(N unresolved + M resolved)` summary, so a prior deferred/blocked
        // item auto-resurfaces into the base's context at each task/phase start
        // instead of relying on it to re-read `docs/decisions/OPEN-DECISIONS.md`.
        // The paired RECORD directive is in the stable head above. Volatile (it
        // changes as items are added/resolved) → placed AFTER the STABLE boundary
        // next to the recorded facts. Bounded ([`DECISIONS_FIRMWARE_BUDGET`] +
        // item cap) + fail-open: no register / a malformed register / no open
        // items → empty, spending nothing (0 recall tokens on a fresh project).
        let open_decisions = crate::open_decisions::decisions_recall_block(
            root,
            crate::open_decisions::DECISIONS_FIRMWARE_BUDGET,
        );
        if !open_decisions.trim().is_empty() {
            fw.push_block(&open_decisions);
        }

        // ── App RUNTIME MODEL — keep it the user's choice, not the dev base's ──
        // When this build's app will itself call an LLM at RUNTIME (a chatbot / RAG
        // service / AI assistant), the base — left unguided — tends to hardcode the
        // BUILT APP's runtime engine to the same vendor it is itself (Anthropic /
        // Claude). The dev base and the app's runtime model are two DIFFERENT things;
        // this block tells the base to treat the app's runtime model + API as a
        // USER-CONFIGURABLE choice (env-driven provider layer: model id + base URL +
        // key var), DEFAULT it to whatever the user named in the requirement, and
        // NEVER silently hardcode the dev base's provider. Pure string analysis (no
        // I/O), part of the always-on work-class head, and EMPTY for a non-AI build —
        // so a plain CRUD product spends no tokens on it. Fail-open by construction.
        let app_llm = crate::app_runtime::runtime_model_directive(requirement);
        if !app_llm.trim().is_empty() {
            fw.push_block(&app_llm);
        }
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
    //
    // ── Layer 4: pitfall memory ── recorded pitfalls matching this project's
    // tech-stack fingerprint + the requirement ("what bit us here" beats "a
    // relevant standard"). ── Layer 5: JIT knowledge ── a small top-K digest of
    // the curated corpus for the requirement (never the whole corpus).
    //
    // Layers 3-5 each do BLOCKING fs / regex / BM25 I/O (repo_map can walk
    // thousands of files; knowledge loads + ranks the corpus). Running them inline
    // on a Tokio worker stalled that worker — and the first response — for hundreds
    // of ms to seconds on a cold cache. Hoist all three onto the blocking pool in
    // ONE `spawn_blocking` so the async runtime stays free. Fail-open: a join error
    // (panicked layer) collapses to empty layers, never blocking the turn.
    let want_repo = route_wants_repo_map(route);
    let want_jit = tier.wants_jit();
    let root_buf = root.to_path_buf();
    let scope = route.scope.clone();
    let req = requirement.to_string();
    let (repo_map, memory, knowledge) = tokio::task::spawn_blocking(move || {
        let repo_map = if want_repo {
            repo_map_layer(&root_buf, &scope)
        } else {
            String::new()
        };
        let memory = if want_jit {
            memory_layer(&root_buf, &req)
        } else {
            String::new()
        };
        let knowledge = if want_jit {
            knowledge_layer(&root_buf, &req)
        } else {
            String::new()
        };
        (repo_map, memory, knowledge)
    })
    .await
    .unwrap_or_default();
    if !repo_map.trim().is_empty() {
        fw.push_block(&repo_map);
    }
    if !memory.trim().is_empty() {
        fw.push_block(&memory);
    }
    if !knowledge.trim().is_empty() {
        fw.push_block(&knowledge);
    }

    fw.finish()
}

/// Build the identity layer: the always-on director identity plus, when the
/// route names a seat (the first of the convened team), that seat's persona — so
/// a frontend build opens "you are the director AND a senior frontend engineer".
/// Generalised (no external source); short by construction.
/// The always-on output-language directive: the base must answer in the user's
/// interface language (the i18n locale), not silently default to English. Returns
/// empty for an English locale (the base's own default — no tokens spent). Naming
/// the target language in English keeps the instruction reliable for every base;
/// the native name reinforces it.
fn language_directive() -> String {
    use umadev_i18n::Lang;
    let (english_name, native) = match umadev_i18n::current() {
        Lang::ZhCn => ("Simplified Chinese", "简体中文"),
        Lang::ZhTw => ("Traditional Chinese", "繁體中文"),
        Lang::En => return String::new(),
    };
    format!(
        "## Output language\n\
         Respond to the user in {english_name} ({native}) — ALL prose: explanations, \
         plans, summaries, questions, status, and progress notes. Keep source code, \
         identifiers, file paths, shell commands, and established technical terms in \
         their original form. {native} is the user's interface language and OVERRIDES \
         any default to English."
    )
}

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

    #[tokio::test]
    async fn user_edited_charter_feeds_into_a_work_turn_but_not_chat() {
        // Wave C firmware link: a user-EDITED `.umadev/constitution.md` surfaces in
        // the work-class head so the team visibly operates by it; pure chat stays
        // light (no charter); and a turn with no charter file is unchanged.
        let tmp = tempfile::TempDir::new().unwrap();
        let cdir = tmp.path().join(".umadev");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(
            cdir.join("constitution.md"),
            "# My team rules\n\n- We pair on every PR.\n",
        )
        .unwrap();

        // Work turn → the user's charter is injected.
        let build = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &build, "做个登录页").await;
        assert!(
            fw.contains("TEAM CHARTER") && fw.contains("pair on every PR"),
            "work turn carries the user-edited charter: {fw}"
        );

        // Pure chat → no charter (stays light).
        let chat = route(RouteClass::Chat, Depth::Fast, Vec::new());
        let fw_chat = compose_firmware(tmp.path(), &chat, "你好").await;
        assert!(
            !fw_chat.contains("TEAM CHARTER"),
            "chat must not carry the charter: {fw_chat}"
        );
    }

    #[tokio::test]
    async fn pristine_or_absent_charter_adds_no_firmware_block() {
        // No charter file, and a pristine generated default, must both inject
        // NOTHING — the built-in craft/anti-slop law already covers them.
        let tmp = tempfile::TempDir::new().unwrap();
        let build = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw_absent = compose_firmware(tmp.path(), &build, "build a thing").await;
        assert!(
            !fw_absent.contains("TEAM CHARTER"),
            "absent → no charter block"
        );

        // Generate the pristine default, then recompose: still no extra block.
        let _ = crate::constitution::ensure_constitution(tmp.path());
        let fw_default = compose_firmware(tmp.path(), &build, "build a thing").await;
        assert!(
            !fw_default.contains("TEAM CHARTER"),
            "pristine default must not be re-injected: {fw_default}"
        );
    }

    #[tokio::test]
    async fn recorded_facts_are_recalled_into_a_work_turn_with_record_guidance() {
        // The memory-loss fix: a fact recorded on this project is recalled into the
        // firmware on a later work turn (so the base never re-searches it), AND the
        // block carries the record-guidance so the base can persist new facts.
        let tmp = tempfile::TempDir::new().unwrap();
        crate::project_facts::record_fact(
            tmp.path(),
            crate::project_facts::Fact::new("JDK17", "/usr/lib/jvm/jdk-17", Some("path")),
        );
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "用 JDK 编译并打包").await;
        assert!(
            fw.contains("KNOWN PROJECT FACTS"),
            "work turn recalls the facts block: {fw}"
        );
        assert!(
            fw.contains("/usr/lib/jvm/jdk-17"),
            "the resolved fact is recalled verbatim: {fw}"
        );
        assert!(
            fw.contains(crate::project_facts::FACTS_REL_PATH),
            "record-guidance (the store path) is present: {fw}"
        );
    }

    #[tokio::test]
    async fn pure_chat_does_not_carry_the_facts_block() {
        // Pure chat stays light (no retrieval) — even with facts on disk, a chat turn
        // carries no facts block, matching the repo-map/knowledge gating.
        let tmp = tempfile::TempDir::new().unwrap();
        crate::project_facts::record_fact(
            tmp.path(),
            crate::project_facts::Fact::new("JDK17", "/usr/lib/jvm/jdk-17", Some("path")),
        );
        let r = route(RouteClass::Chat, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "你好,在吗?").await;
        assert!(
            !fw.contains("KNOWN PROJECT FACTS"),
            "chat must not carry the facts block: {fw}"
        );
    }

    #[tokio::test]
    async fn no_facts_means_no_facts_block() {
        // Fail-open / first-ever turn: with no fact store, a work turn carries no
        // facts block (behaves exactly as before this feature).
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个登录页").await;
        assert!(
            !fw.contains("KNOWN PROJECT FACTS"),
            "no store → no facts block: {fw}"
        );
    }

    /// Seed a committed open-decisions register with two unresolved items + one
    /// resolved item.
    fn seed_open_decisions(root: &Path) {
        let path = root.join(crate::open_decisions::REGISTER_REL_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "# Open Decisions Register\n\n\
             ## OPEN — waiting-on-external-condition — Stripe live key not provisioned\n\
             - **Open item**: cannot wire live payments without STRIPE_LIVE_KEY\n\
             - **Resolves when**: the STRIPE_LIVE_KEY env var is available\n\n\
             ## OPEN — design-decision-to-evaluate — Session store cookie vs Redis\n\
             - **Open item**: pick the session backend\n\n\
             ## RESOLVED — existing-design-boundary — Single-region deploy accepted\n\
             - **Resolution**: single region for v1\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn work_turn_recalls_open_decisions_and_carries_the_directive() {
        // The decision-loss fix: on a WORK turn the firmware RECALLS the still-
        // unresolved parking-lot items (prefixed with the N/M summary) AND carries
        // the always-on record-to-register DIRECTIVE (categories + fields).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_open_decisions(tmp.path());
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "接着做支付和会话").await;
        // RECALL: the unresolved items + the "(N unresolved + M resolved)" summary.
        assert!(
            fw.contains("2 unresolved + 1 resolved"),
            "recall carries the N/M summary: {fw}"
        );
        assert!(fw.contains("Stripe live key"), "recalls open item 1: {fw}");
        assert!(fw.contains("cookie vs Redis"), "recalls open item 2: {fw}");
        assert!(
            !fw.contains("Single-region deploy"),
            "resolved item not recalled: {fw}"
        );
        // DIRECTIVE: the record-to-register guidance with the categories + fields.
        assert!(
            fw.contains(crate::open_decisions::REGISTER_REL_PATH),
            "directive names the register path: {fw}"
        );
        assert!(
            fw.contains("waiting-on-external-condition")
                && fw.contains("design-decision-to-evaluate")
                && fw.contains("existing-design-boundary"),
            "directive documents the three categories: {fw}"
        );
        assert!(
            fw.contains("**Resolves when**") && fw.contains("**Open item**"),
            "directive documents the structured fields: {fw}"
        );
    }

    #[tokio::test]
    async fn pure_chat_carries_no_open_decisions_block() {
        // Pure chat stays light: even with a register on disk, a chat turn carries
        // NEITHER the recall NOR the directive (0 tokens on chat/trivial).
        let tmp = tempfile::TempDir::new().unwrap();
        seed_open_decisions(tmp.path());
        let r = route(RouteClass::Chat, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "你好,在吗?").await;
        assert!(
            !fw.contains("OPEN DECISIONS") && !fw.contains("OPEN-DECISIONS DISCIPLINE"),
            "chat carries no open-decisions block: {fw}"
        );
        assert!(
            !fw.contains("2 unresolved"),
            "chat carries no recall summary: {fw}"
        );
    }

    #[tokio::test]
    async fn open_decisions_is_fail_open_on_a_missing_register() {
        // No register (a fresh project): a work turn still carries the always-on
        // DIRECTIVE (so the base records its FIRST open item) but NO recall — and
        // never panics.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个登录页").await;
        assert!(
            fw.contains("OPEN-DECISIONS DISCIPLINE"),
            "directive is always-on for a work turn: {fw}"
        );
        assert!(
            !fw.contains("OPEN DECISIONS — unresolved"),
            "no register → no recall block: {fw}"
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

    #[tokio::test]
    async fn ai_app_build_carries_the_runtime_model_directive() {
        // A build whose app calls an LLM at RUNTIME must carry the
        // app-runtime-model-is-configurable instruction: never silently hardcode the
        // dev base's vendor (Claude) as the app's runtime engine.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个智能客服聊天机器人").await;
        assert!(
            fw.contains("App runtime model — USER-CONFIGURABLE"),
            "AI-app build carries the runtime-model directive: {fw}"
        );
        assert!(
            fw.contains("ANTHROPIC_API_KEY") || fw.contains("Anthropic / Claude"),
            "directive names the vendor not to hardcode: {fw}"
        );
        assert!(
            fw.to_lowercase().contains("openai-compatible"),
            "directive offers the OpenAI-compatible provider layer: {fw}"
        );
    }

    #[tokio::test]
    async fn ai_app_build_threads_an_explicit_runtime_model() {
        // When the requirement NAMES a runtime model, the firmware threads it as the
        // default the app should be configured for.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::BackendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个聊天机器人,运行时用千问 Max").await;
        assert!(
            fw.contains("NAMED a runtime model") && fw.contains("Qwen"),
            "explicit runtime model is detected + threaded into the firmware: {fw}"
        );
    }

    #[tokio::test]
    async fn plain_build_does_not_carry_the_runtime_model_directive() {
        // An ordinary CRUD product (no runtime LLM) must NOT carry the directive —
        // no wasted tokens, and the gap fix stays scoped to AI apps.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );
        let fw = compose_firmware(tmp.path(), &r, "做一个待办事项 SaaS 产品").await;
        assert!(
            !fw.contains("App runtime model — USER-CONFIGURABLE"),
            "plain build must not carry the runtime-model directive: {fw}"
        );
    }

    #[tokio::test]
    async fn pure_chat_skips_the_runtime_model_directive() {
        // Even an AI-flavoured chat turn stays light: the directive is a work-class
        // head block, so pure chat carries no runtime-model directive.
        let tmp = tempfile::TempDir::new().unwrap();
        let r = route(RouteClass::Chat, Depth::Fast, Vec::new());
        let fw = compose_firmware(tmp.path(), &r, "聊天机器人一般怎么做?").await;
        assert!(
            !fw.contains("App runtime model — USER-CONFIGURABLE"),
            "chat must not carry the runtime-model directive: {fw}"
        );
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

    #[tokio::test]
    async fn stable_prefix_is_a_byte_identical_leading_prefix() {
        // KV-CACHE INVARIANT: the firmware's stable head (identity → output-language →
        // craft → anti-slop → charter) must lead and be byte-identical regardless of
        // the per-turn volatile inputs. A head-only compose (a blank repo: no facts, no
        // repo-map, no lessons, no knowledge) must therefore be an EXACT leading prefix
        // of a compose that carries a full volatile tail — proving nothing volatile is
        // interleaved above the stable head (which would bust the base's KV-cache).
        let _no_corpus = crate::test_support::NoBundledCorpus::new();
        // A non-AI build so the app-runtime directive is empty in BOTH (kept identical).
        let req = "做一个待办事项 SaaS 产品";
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );

        // Compose A — blank repo → the pure stable head, no volatile tail.
        let head_dir = tempfile::TempDir::new().unwrap();
        let head = compose_firmware(head_dir.path(), &r, req).await;

        // Compose B — same route/config, but a full volatile tail: brownfield code
        // (repo-map) + a recorded fact + matching curated knowledge.
        let full_dir = tempfile::TempDir::new().unwrap();
        seed_brownfield(full_dir.path());
        crate::project_facts::record_fact(
            full_dir.path(),
            crate::project_facts::Fact::new("port", "8080", Some("port")),
        );
        let kd = full_dir.path().join("knowledge/frontend");
        std::fs::create_dir_all(&kd).unwrap();
        std::fs::write(
            kd.join("std.md"),
            "# Frontend\n\n## Design tokens\n\nUse design tokens and component states for a SaaS todo product.",
        )
        .unwrap();
        let full = compose_firmware(full_dir.path(), &r, req).await;

        // The volatile tail must actually be present (else the test proves nothing)…
        assert!(
            full.chars().count() > head.chars().count(),
            "compose B must carry a volatile tail"
        );
        assert!(
            full.contains("YOUR CODEBASE") || full.contains("KNOWN PROJECT FACTS"),
            "compose B carries volatile blocks: {full}"
        );
        // …and the stable head must be a BYTE-EXACT leading prefix of it.
        assert!(
            full.starts_with(&head),
            "the stable head must be a byte-identical leading prefix:\nHEAD=<<{head}>>\nFULL=<<{full}>>"
        );
        // Sanity: the head actually contains the stable blocks it claims to.
        assert!(head.to_lowercase().contains("umadev"), "identity in head");
        assert!(head.contains("HOW YOUR TEAM BUILDS"), "craft in head");
        assert!(head.contains("ANTI-AI-SLOP"), "anti-slop in head");
    }

    #[tokio::test]
    async fn stable_prefix_holds_across_two_different_volatile_tails() {
        // The literal lock: two work turns with the SAME route/config but DIFFERENT
        // volatile inputs (different repo-map + different facts) must share a byte-
        // identical leading prefix THROUGH the last stable block (the anti-slop law) —
        // the volatile boundary. Only what comes after it may differ.
        let _no_corpus = crate::test_support::NoBundledCorpus::new();
        let req = "做一个待办事项 SaaS 产品";
        let r = route(
            RouteClass::Build,
            Depth::Standard,
            vec![Seat::FrontendEngineer],
        );

        // Repo A — TS checkout/auth + a port fact.
        let a_dir = tempfile::TempDir::new().unwrap();
        seed_brownfield(a_dir.path());
        crate::project_facts::record_fact(
            a_dir.path(),
            crate::project_facts::Fact::new("port", "8080", Some("port")),
        );
        let a = compose_firmware(a_dir.path(), &r, req).await;

        // Repo B — a totally different file + a different fact.
        let b_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            b_dir.path().join("payments.go"),
            "package main\nfunc ChargeCard() {}\nfunc Refund() {}\n",
        )
        .unwrap();
        crate::project_facts::record_fact(
            b_dir.path(),
            crate::project_facts::Fact::new("jdk", "/opt/jdk-17", Some("path")),
        );
        let b = compose_firmware(b_dir.path(), &r, req).await;

        // The two composes DIFFER (different volatile tails)…
        assert_ne!(a, b, "the two composes must carry different volatile tails");
        // …but the stable prefix THROUGH the anti-slop law is byte-identical.
        let anchor = a.find(ANTI_SLOP_LAW).expect("anti-slop law present") + ANTI_SLOP_LAW.len();
        assert_eq!(
            a.as_bytes().get(..anchor),
            b.as_bytes().get(..anchor),
            "the stable prefix up to the volatile boundary must be byte-identical"
        );
    }

    #[test]
    fn firmware_budget_constants_are_bounded_and_sane() {
        // CONTEXT BUDGET: the firmware is an OVERLAY on top of the base's own (large)
        // system prompt + the per-turn directive, so it must stay a small high-signal
        // slice and leave most of the window for the actual work. Lock the
        // relationships at COMPILE time so a future edit can't quietly let the firmware
        // crowd out the work (a `const {}` assertion fails the build, not just a run).
        const {
            // The firmware stays a small overlay, not a second corpus.
            assert!(FIRMWARE_BUDGET <= 16_000);
            // The JIT tail reserve is a fraction of the whole budget — the stable head
            // (identity + craft + law) always has room to lead.
            assert!(ALWAYS_ON_RESERVE < FIRMWARE_BUDGET);
            // The repo-map slice is ONE part of the JIT tail, not all of it.
            assert!(REPO_MAP_BUDGET < ALWAYS_ON_RESERVE);
            // The user charter is a bounded slice of the head.
            assert!(CONSTITUTION_BUDGET < FIRMWARE_BUDGET);
        }
    }
}
