//! **Which design archetype fits THIS product?** — the one question, asked of the brain.
//!
//! UmaDev's design system is ON BY DEFAULT: even when the base's UIUX doc declares no
//! visual direction, the frontend still gets a binding token contract, so *some* archetype
//! (premium-luxury / brutalist-bold / glass-aurora / soft-warm / editorial-clean / …) has to
//! be chosen. That choice is a DESIGNER'S JUDGMENT about the product's domain, audience, and
//! the tone it must project.
//!
//! It used to be a keyword classifier — a product-type inference engine that scanned the
//! requirement against hardcoded trigger lists (`fashion`/`portfolio` → brutalist,
//! `wealth`/`watch` → luxury, and so on). That is the wrong shape of answer, for the same
//! reason a lexical color-permission reader was: a product's character has unboundedly many
//! phrasings, and the next requirement is always outside the list. "a calm space for grief
//! journaling" chooses no keyword yet clearly wants soft-warm; "the terminal for your money"
//! reads as tech-utility to a designer and as `payment`→bold-geometric to a `contains()`.
//! **A word list is the wrong shape of answer.**
//!
//! ## The project's standing rule: judgment is the brain's, not a table's
//!
//! UmaDev does not classify intent with keywords. The resident chat turn opens on a FIXED,
//! lightweight seed and a build is recognised REACTIVELY — the instant the base writes a
//! real file, `react_to_first_write` latches it — never by a pre-action word-table verdict.
//! Where a genuine model judgement is still owed it is ONE stateless, structured consult, and
//! [`crate::color_permission`] is the sibling example: it asks the base's own model whether
//! the requirement authorized a flagged hue. *"Which archetype fits this product?"* is the
//! same class of question, and it gets the same answer: **one stateless, structured consult →
//! a typed [`DesignArchetype`]**, an id drawn from the fixed, known archetype set. No
//! product-type inference engine, and nothing that grows.
//!
//! Only the *choice of archetype* moves to the brain. The design tokens, the archetype
//! knowledge packs, and the anti-AI-slop token conformance downstream are UmaDev's core value
//! and are untouched: this module changes only HOW the archetype is picked, never what the
//! picked archetype then binds.
//!
//! ## Fail direction: fall back to the SAME deterministic default, never crash
//!
//! Unlike the color floor, an undetermined archetype is NOT a safety stand-down — it is a
//! missing recommendation. So the strict direction here is the CONSERVATIVE DEFAULT the code
//! already used: brain unreachable, offline runtime, fork refused, prose instead of JSON, a
//! timeout, or an id outside the known set → [`DesignArchetype::undetermined`], which persists
//! nothing, and the sync coach renderer falls back to the deterministic keyword recommendation
//! (`coach::recommend_design_system`, now kept ONLY as that fail-fallback). The
//! pipeline is never blocked and nothing ever crashes.
//!
//! ## Computed ONCE at the run door, persisted, read by the sync renderer
//!
//! The consult runs at the run door where the phase-walk pipeline persists its governance
//! context ([`crate::continuous::run_block`], which both the legacy pipeline and the
//! `AgentRunner` continuous path funnel through) — the same door where the colour question is
//! asked, the instant the requirement is known. The chosen id is written into
//! `.umadev/design-archetype.json`, stamped with the requirement it was derived from. (The
//! director-loop `/run` engine never renders per-phase design injection, so it neither needs
//! nor asks the archetype question — only the colour question, which every code-writing path
//! must persist for the write governor.) The archetype is *resolved* in
//! `coach::load_design_system_inject`, a synchronous prompt renderer with no brain of
//! its own; it READS the stored pick as its fallback (after an explicit `/design` and after the
//! UIUX doc's own declared direction, both of which still win). One consult, one writer, one
//! reader.
//!
//! The stored pick is honoured only for the SAME requirement it was derived from
//! (provenance-gated by [`umadev_governance::requirement_fingerprint`]): a different
//! requirement — or an unstamped / unreadable / absent file — carries nothing forward and the
//! renderer falls back to the deterministic recommendation.

use std::path::Path;

use umadev_runtime::BaseSession;

/// The brain's verdict on which design archetype fits the requirement — a typed answer to one
/// question.
///
/// `archetype` is `Some(id)` only when the brain returned one of the KNOWN archetype ids
/// (`coach::DESIGN_ARCHETYPES`); every failure path (no brain, a timeout, prose
/// instead of JSON, an id outside the set) yields `None`, which the caller treats as "no
/// recommendation — use the deterministic fallback", never as an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesignArchetype {
    /// The archetype the brain chose, guaranteed to be one of
    /// `coach::DESIGN_ARCHETYPES`, or `None` when the brain gave no usable answer.
    ///
    /// `None` means *undetermined* — both "the brain was unavailable" and "the brain answered
    /// something we cannot use". The renderer treats those identically: fall back to the
    /// deterministic keyword recommendation.
    pub archetype: Option<String>,
    /// Why — the brain's own short justification, or the reason the pick was left undetermined.
    /// Diagnostic only; it drives no logic.
    pub reason: String,
}

impl DesignArchetype {
    /// A concrete pick — `archetype` MUST already be validated as a known id by the caller.
    #[must_use]
    pub fn chosen(archetype: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            archetype: Some(archetype.into()),
            reason: reason.into(),
        }
    }

    /// No usable pick — the renderer falls back to the deterministic recommendation. Every
    /// failure path lands here.
    #[must_use]
    pub fn undetermined(reason: impl Into<String>) -> Self {
        Self {
            archetype: None,
            reason: reason.into(),
        }
    }
}

impl Default for DesignArchetype {
    /// Undetermined, un-consulted — the deterministic-fallback posture.
    fn default() -> Self {
        Self::undetermined("not consulted")
    }
}

/// The fixed archetype menu handed to the brain: each known id paired with the KIND of product
/// it suits. This is descriptive guidance the model reasons over, not a `contains()` table —
/// the ids are the same closed set the renderer binds, and the drift test keeps them identical
/// to [`crate::coach::DESIGN_ARCHETYPES`].
const ARCHETYPE_MENU: &[(&str, &str)] = &[
    (
        "modern-minimal",
        "geometric sans-serif, precise spacing, monochrome with one accent — SaaS, dev tools, \
         productivity apps, dashboards, general-purpose software; also the versatile default \
         when nothing more specific fits",
    ),
    (
        "editorial-clean",
        "magazine-like, serif-accent headings, generous whitespace, photography-driven — \
         content sites, blogs, documentation, publishing, newsletters, media",
    ),
    (
        "tech-utility",
        "monospace accents, dense information, dark-mode-native — developer tools, data / \
         monitoring / analytics platforms, admin & infra consoles, terminals, log viewers",
    ),
    (
        "soft-warm",
        "rounded corners, warm palette, friendly and approachable — consumer apps, education, \
         health & wellness, community, lifestyle, social, family",
    ),
    (
        "bold-geometric",
        "high contrast, oversized type, asymmetric grid — marketing / landing pages, brand & \
         product launches, fintech, gaming",
    ),
    (
        "brutalist-bold",
        "raw, expressive, unconventional, editorial-experimental — creative agencies, \
         portfolios, studios, fashion, music, art, culture",
    ),
    (
        "glass-aurora",
        "luminous glass surfaces, aurora gradients, futuristic — AI / generative products, \
         web3 / crypto / blockchain, forward-looking sci-tech",
    ),
    (
        "premium-luxury",
        "restrained, spacious, refined, high-end — luxury goods, wealth & private banking, \
         automotive, watches, jewellery, membership & flagship experiences",
    ),
];

/// Compose the one structured question. Terse and decisive: the model is told this is a
/// designer's judgment over the WHOLE requirement, handed the closed menu of ids, and required
/// to answer with exactly one of them (defaulting to `modern-minimal` when nothing leans) — so
/// an out-of-set answer can only be a malformed reply, which the parser turns into a fallback.
fn archetype_system_prompt() -> String {
    let mut menu = String::new();
    for (id, intent) in ARCHETYPE_MENU {
        menu.push_str(&format!("- {id}: {intent}\n"));
    }
    format!(
        "You choose ONE design archetype for a software product, and nothing else. \
         A product's archetype is a DESIGNER'S JUDGMENT about its domain, audience, and the \
         tone it must project — read the requirement as a whole, do not match keywords. \
         Choose from EXACTLY these archetypes (id: the kind of product it suits):\n\
         {menu}\n\
         Pick the SINGLE id whose character the product should have. If the requirement does \
         not lean toward any of the more specific archetypes, choose `modern-minimal` (the \
         versatile default). NEVER invent an id outside the list above; the two scripts \
         (English and 中文) are judged identically. \
         Reply with EXACTLY ONE JSON object and nothing else — no markdown, no code fence, no \
         prose: {{\"archetype\":\"<one id from the list>\",\"reason\":\"<one short clause>\"}}"
    )
}

/// Ask the brain which known archetype best fits `requirement`.
///
/// One read-only forked consult, one strict-JSON answer, one typed pick — the SAME plumbing
/// [`crate::color_permission::consult_color_permission`] uses (`continuous::fork_with_timeout`
/// → `continuous::ForkConsult` → `judge_json`). The main (single-writer) session is
/// never touched, and nothing on disk changes — the caller persists the result.
///
/// **Fail to the deterministic default at every edge** (see the module docs): an empty
/// requirement, a fork that will not open, an offline brain, a timeout, prose instead of JSON,
/// a malformed object, or an id outside the known set all return
/// [`DesignArchetype::undetermined`]. Never errors, never panics, never blocks the host.
pub async fn consult_design_archetype(
    session: &mut dyn BaseSession,
    requirement: &str,
) -> DesignArchetype {
    if requirement.trim().is_empty() {
        return DesignArchetype::undetermined("empty requirement — deterministic fallback");
    }

    let fork = crate::continuous::fork_with_timeout(session).await;
    let consult = crate::continuous::ForkConsult::new(fork);
    let answer = consult
        .judge_json(
            "design-archetype",
            &archetype_system_prompt(),
            format!("Requirement:\n{requirement}"),
        )
        .await;
    consult.end().await;

    let Some(json) = answer else {
        return DesignArchetype::undetermined(
            "the brain gave no usable archetype — deterministic fallback",
        );
    };
    parse_archetype(&json).unwrap_or_else(|| {
        DesignArchetype::undetermined(
            "the brain's archetype could not be parsed or was outside the known set — \
             deterministic fallback",
        )
    })
}

/// Parse the brain's strict-JSON verdict. `None` unless `archetype` is present, a string, and
/// one of the KNOWN ids — an unknown id is not a pick, it is a malformed answer, and the caller
/// falls back to the deterministic recommendation rather than binding an id nothing seeds.
fn parse_archetype(json: &str) -> Option<DesignArchetype> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let id = v.get("archetype")?.as_str()?.trim();
    if !crate::coach::DESIGN_ARCHETYPES.contains(&id) {
        return None;
    }
    let reason = v
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(200)
        .collect::<String>();
    Some(DesignArchetype::chosen(
        id,
        if reason.is_empty() {
            "the brain gave no reason".to_string()
        } else {
            reason
        },
    ))
}

/// Workspace-relative path of the persisted archetype pick.
const DESIGN_ARCHETYPE_REL: &str = ".umadev/design-archetype.json";
const MAX_DESIGN_ARCHETYPE_BYTES: usize = 1024 * 1024;

/// UNIX seconds, or 0 when the clock is unreadable.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Persist the brain's archetype pick for `requirement` to [`DESIGN_ARCHETYPE_REL`], stamped
/// with the requirement's fingerprint so a later run for a DIFFERENT requirement cannot inherit
/// it.
///
/// An [`DesignArchetype::undetermined`] verdict writes NOTHING — there is no recommendation to
/// record, and leaving the file absent (or an older matching-requirement pick in place) makes
/// the reader fall back to the deterministic recommendation. Fail-open: an unwritable
/// `.umadev/` is swallowed.
pub(crate) fn persist_design_archetype(
    project_root: &Path,
    requirement: &str,
    decision: &DesignArchetype,
) {
    let Some(id) = decision.archetype.as_deref() else {
        return;
    };
    let Some(key) = umadev_state::privacy::installation_key() else {
        return;
    };
    let payload = serde_json::json!({
        "archetype": id,
        "requirement_fingerprint": umadev_governance::requirement_fingerprint(&key, requirement),
        "derived_at": now_secs(),
    });
    let Ok(dir) =
        crate::bounded_fs::ensure_real_dir_beneath(project_root, std::path::Path::new(".umadev"))
    else {
        return;
    };
    if let Ok(json) = serde_json::to_vec_pretty(&payload) {
        if json.len() <= MAX_DESIGN_ARCHETYPE_BYTES {
            let _ = umadev_state::fs::atomic_write(&dir.join("design-archetype.json"), &json);
        }
    }
}

/// The archetype pick ALREADY RECORDED for `requirement` in this workspace, or `None`.
///
/// Provenance-gated: the stored id is honoured only when its fingerprint matches THIS
/// requirement (so a previous product's archetype cannot leak into a new build), and only when
/// it is still one of the known ids. A mismatch / unstamped / unreadable / absent file, or an
/// id no longer known, yields `None` and the renderer falls back to the deterministic
/// recommendation.
pub(crate) fn stored_design_archetype(project_root: &Path, requirement: &str) -> Option<String> {
    let raw = crate::bounded_fs::read_utf8_beneath(
        project_root,
        &project_root.join(DESIGN_ARCHETYPE_REL),
        MAX_DESIGN_ARCHETYPE_BYTES,
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let key = umadev_state::privacy::installation_key()?;
    let stored: [u8; 32] =
        serde_json::from_value(v.get("requirement_fingerprint")?.clone()).ok()?;
    if stored != umadev_governance::requirement_fingerprint(&key, requirement) {
        return None;
    }
    let id = v.get("archetype")?.as_str()?.trim();
    if crate::coach::DESIGN_ARCHETYPES.contains(&id) {
        Some(id.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tempfile::TempDir;
    use umadev_runtime::{ApprovalDecision, SessionError, SessionEvent, TurnStatus};

    // ── A stub brain. The main session is never driven — only forked. ────────────────

    struct StubFork {
        reply: String,
        pending: VecDeque<SessionEvent>,
    }
    #[async_trait::async_trait]
    impl BaseSession for StubFork {
        async fn send_turn(&mut self, _d: String) -> Result<(), SessionError> {
            self.pending = [
                SessionEvent::TextDelta(self.reply.clone()),
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            ]
            .into_iter()
            .collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.pending.pop_front()
        }
        async fn respond(&mut self, _r: &str, _d: ApprovalDecision) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    /// A base whose read-only fork answers the archetype question with `reply`.
    /// `can_fork: false` is the unreachable-brain path.
    struct StubBrain {
        reply: String,
        can_fork: bool,
    }
    impl StubBrain {
        fn answering(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                can_fork: true,
            }
        }
        fn unreachable() -> Self {
            Self {
                reply: String::new(),
                can_fork: false,
            }
        }
        fn picking(id: &str) -> Self {
            Self::answering(&format!(
                "{{\"archetype\":\"{id}\",\"reason\":\"judged from the requirement\"}}"
            ))
        }
    }
    #[async_trait::async_trait]
    impl BaseSession for StubBrain {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            if !self.can_fork {
                return Err(SessionError::ForkUnsupported("test".into()));
            }
            Ok(Box::new(StubFork {
                reply: self.reply.clone(),
                pending: VecDeque::new(),
            }))
        }
        async fn send_turn(&mut self, _d: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            None
        }
        async fn respond(&mut self, _r: &str, _d: ApprovalDecision) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    #[test]
    fn the_menu_ids_are_exactly_the_known_archetype_set() {
        // The prompt's closed menu must never drift from the ids the renderer binds — a menu id
        // with no seeded design system, or a bindable id absent from the menu, would leave the
        // brain unable to pick it. Keep them identical.
        let menu: Vec<&str> = ARCHETYPE_MENU.iter().map(|(id, _)| *id).collect();
        assert_eq!(menu.as_slice(), crate::coach::DESIGN_ARCHETYPES);
    }

    #[test]
    fn the_prompt_asks_for_judgment_over_the_closed_menu() {
        // The prompt is the contract: it frames the choice as a judgment, forbids inventing an
        // id, names the default, and lists every bindable archetype.
        let p = archetype_system_prompt();
        assert!(p.contains("DESIGNER'S JUDGMENT"));
        assert!(p.contains("do not match keywords"));
        assert!(p.contains("NEVER invent an id"));
        assert!(p.contains("modern-minimal"));
        for id in crate::coach::DESIGN_ARCHETYPES {
            assert!(p.contains(id), "the prompt must offer `{id}`");
        }
        assert!(p.contains("archetype")); // fixes the JSON shape the parser reads
    }

    #[tokio::test]
    async fn a_pick_from_the_menu_is_returned_for_every_known_id() {
        // Faithful path: whatever known id the brain returns is the id that lands.
        for id in crate::coach::DESIGN_ARCHETYPES {
            let mut brain = StubBrain::picking(id);
            let got = consult_design_archetype(&mut brain, "some product").await;
            assert_eq!(got.archetype.as_deref(), Some(*id));
        }
    }

    #[tokio::test]
    async fn an_unreachable_brain_is_undetermined() {
        // No fork ⇒ no pick ⇒ deterministic fallback. Never a crash, never a guess here.
        let mut brain = StubBrain::unreachable();
        let got = consult_design_archetype(&mut brain, "a data monitoring dashboard").await;
        assert!(got.archetype.is_none());
    }

    #[tokio::test]
    async fn a_malformed_or_unknown_verdict_is_undetermined() {
        // Every way the brain can fail the question is the SAME answer: undetermined, so the
        // renderer takes the deterministic recommendation. An id outside the known set is
        // treated as malformed — we never bind an archetype nothing seeds.
        for reply in [
            "",                                   // silence
            "Sure! This looks like a dev tool.",  // prose, no JSON
            "{}",                                 // JSON, no decision
            "{\"archetype\":123}",                // wrong type
            "{\"kind\":\"tech-utility\"}",        // wrong key
            "{\"archetype\":\"neon-cyberpunk\"}", // id outside the known set
            "{\"archetype\":\"tech-utility\"",    // truncated
        ] {
            let mut brain = StubBrain::answering(reply);
            let got = consult_design_archetype(&mut brain, "a product").await;
            assert!(
                got.archetype.is_none(),
                "an unusable verdict is undetermined: {reply:?}"
            );
        }
        // …and a clean pick still lands.
        let mut brain = StubBrain::picking("premium-luxury");
        assert_eq!(
            consult_design_archetype(&mut brain, "a product")
                .await
                .archetype
                .as_deref(),
            Some("premium-luxury")
        );
    }

    #[tokio::test]
    async fn an_empty_requirement_costs_no_consult() {
        // Nothing to judge → undetermined without asking (the stub would GRANT, yet the answer
        // is still undetermined, proving it was not consulted).
        let mut brain = StubBrain::picking("brutalist-bold");
        let got = consult_design_archetype(&mut brain, "   ").await;
        assert!(got.archetype.is_none());
    }

    #[test]
    fn a_persisted_pick_round_trips_only_for_the_same_requirement() {
        let tmp = TempDir::new().unwrap();
        let req = "做一个高端腕表电商";
        persist_design_archetype(
            tmp.path(),
            req,
            &DesignArchetype::chosen("premium-luxury", "luxury retail"),
        );
        // Same requirement → the stored pick.
        assert_eq!(
            stored_design_archetype(tmp.path(), req).as_deref(),
            Some("premium-luxury")
        );
        // A different requirement → provenance mismatch → nothing carried forward.
        assert_eq!(
            stored_design_archetype(tmp.path(), "做一个数据监控后台"),
            None
        );
        let raw = std::fs::read_to_string(tmp.path().join(DESIGN_ARCHETYPE_REL)).unwrap();
        assert!(
            !raw.contains(req),
            "the requirement itself is never persisted"
        );
        assert!(raw.contains("requirement_fingerprint"));
        assert!(
            !raw.contains("requirement_hash"),
            "new writes must not retain the enumerable FNV field"
        );

        std::fs::write(
            tmp.path().join(DESIGN_ARCHETYPE_REL),
            r#"{"archetype":"premium-luxury","requirement_hash":123,"derived_at":1800000000}"#,
        )
        .unwrap();
        assert_eq!(
            stored_design_archetype(tmp.path(), req),
            None,
            "legacy enumerable fingerprints are compatible JSON but not trusted"
        );
    }

    #[test]
    fn oversized_persisted_pick_is_ignored() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        std::fs::File::create(tmp.path().join(DESIGN_ARCHETYPE_REL))
            .unwrap()
            .set_len(u64::try_from(MAX_DESIGN_ARCHETYPE_BYTES + 1).unwrap())
            .unwrap();
        assert_eq!(stored_design_archetype(tmp.path(), "a product"), None);
    }

    #[cfg(unix)]
    #[test]
    fn linked_persisted_pick_is_ignored() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        let key = umadev_state::privacy::installation_key().unwrap();
        let payload = serde_json::json!({
            "archetype": "premium-luxury",
            "requirement_fingerprint": umadev_governance::requirement_fingerprint(&key, "a product"),
            "derived_at": now_secs(),
        });
        let outside_path = outside.path().join("pick.json");
        std::fs::write(&outside_path, serde_json::to_vec(&payload).unwrap()).unwrap();
        symlink(&outside_path, tmp.path().join(DESIGN_ARCHETYPE_REL)).unwrap();

        assert_eq!(stored_design_archetype(tmp.path(), "a product"), None);
    }

    #[cfg(unix)]
    #[test]
    fn persisted_pick_never_follows_managed_directory_or_leaf_links() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_pick = outside.path().join("design-archetype.json");
        std::fs::write(&outside_pick, b"outside").unwrap();
        let decision = DesignArchetype::chosen("premium-luxury", "fit");

        symlink(outside.path(), root.path().join(".umadev")).unwrap();
        persist_design_archetype(root.path(), "a product", &decision);
        assert_eq!(std::fs::read(&outside_pick).unwrap(), b"outside");

        std::fs::remove_file(root.path().join(".umadev")).unwrap();
        std::fs::create_dir(root.path().join(".umadev")).unwrap();
        symlink(&outside_pick, root.path().join(DESIGN_ARCHETYPE_REL)).unwrap();
        persist_design_archetype(root.path(), "a product", &decision);
        assert_eq!(std::fs::read(&outside_pick).unwrap(), b"outside");
    }

    #[test]
    fn an_undetermined_verdict_persists_nothing() {
        let tmp = TempDir::new().unwrap();
        let req = "a product";
        persist_design_archetype(tmp.path(), req, &DesignArchetype::undetermined("no brain"));
        assert_eq!(stored_design_archetype(tmp.path(), req), None);
        assert!(!tmp.path().join(DESIGN_ARCHETYPE_REL).exists());
    }

    #[test]
    fn a_stored_unknown_id_is_ignored() {
        // Defense in depth: a hand-edited / stale file whose id is no longer known must not
        // bind — the reader falls back to the deterministic recommendation.
        let tmp = TempDir::new().unwrap();
        let req = "a product";
        std::fs::create_dir_all(tmp.path().join(".umadev")).unwrap();
        let key = umadev_state::privacy::installation_key().unwrap();
        let payload = serde_json::json!({
            "archetype": "neon-cyberpunk",
            "requirement_fingerprint": umadev_governance::requirement_fingerprint(&key, req),
            "derived_at": now_secs(),
        });
        std::fs::write(
            tmp.path().join(DESIGN_ARCHETYPE_REL),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        assert_eq!(stored_design_archetype(tmp.path(), req), None);
    }
}
