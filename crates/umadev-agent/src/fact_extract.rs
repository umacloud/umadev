//! Active fact-extraction **backstop** — UmaDev records durable project facts
//! ITSELF, instead of trusting the base to volunteer them.
//!
//! ## Why this exists (the recording side was unreliable)
//!
//! [`crate::project_facts`] RECALLS durable facts into the firmware on every work
//! turn (so the team never re-searches a fact it already resolved), and the
//! firmware fact block ALSO instructs the base to append new facts to
//! [`crate::project_facts::FACTS_REL_PATH`] with its own file tools. But that
//! RECORDING side depended on the base VOLUNTARILY writing the file — and the base
//! often didn't. So `.umadev/memory/facts.jsonl` never appeared, the store stayed
//! empty, and recall had nothing to surface: the user expected the file and it was
//! absent. Instructing-only is not a guarantee.
//!
//! ## What this module does
//!
//! After a meaningful WORK turn (a build step that completed, a turn that resolved
//! paths / ran build-test commands / established constraints — never pure chat),
//! UmaDev asks the borrowed brain — on a READ-ONLY `fork()`, the SAME seam the
//! critics ([`crate::critics`]) and the brain router ([`crate::router`]) use — to
//! list the durable facts the turn established as concise `key: value` lines (or
//! `none`). It then parses those lines and writes them via
//! [`crate::project_facts::record_facts`] (which dedups by key + enforces the
//! bounds), so the store reliably populates WITHOUT depending on the base writing
//! it. The firmware's record-guidance stays in place — the base CAN still append
//! directly; this is the active backstop that GUARANTEES the file gets populated.
//!
//! ## Bounded + fail-open by contract
//!
//! - **Work-class only.** Pure [`RouteClass::Chat`] / [`RouteClass::Explain`]
//!   establish nothing durable, so they never fork and never spend a token (see
//!   [`route_warrants_extraction`]) — exactly the firmware's work-class gate.
//! - **Throttled.** Even on a long multi-step build the extraction runs only on a
//!   bounded subset of work turns ([`should_extract`]) — once early so a one-step
//!   build still populates the file, then every Nth turn — never once per step.
//! - **Fail-open.** A failed/wedged fork, an offline brain, a timeout, an empty /
//!   `none` reply, an unparseable reply, or an unwritable store all degrade to
//!   "no facts written" (`0`). This module NEVER panics and NEVER returns an error
//!   that could break the turn.

use std::path::Path;
use std::sync::Arc;

use umadev_runtime::BaseSession;

use crate::events::{EngineEvent, EventSink};
use crate::project_facts::{self, Fact};
use crate::router::{RouteClass, RoutePlan};

/// The throttle period: run the active extraction once on the FIRST work turn,
/// then every Nth work turn after that. Small enough that a real build still
/// records facts as they accrue, large enough that a long multi-step build never
/// pays an extraction on every single step. See [`should_extract`].
const EXTRACT_EVERY_N_WORK_TURNS: usize = 3;

/// Hard cap on how many facts a SINGLE extraction may apply, so one runaway reply
/// (a base that dumps a wall of text) can't churn the store. The store has its own
/// [`crate::project_facts`] cap; this just bounds one extraction's contribution.
const MAX_FACTS_PER_EXTRACTION: usize = 24;

/// Whether `route` is a WORK turn that can establish durable facts worth recording.
///
/// Pure [`RouteClass::Chat`] (small talk / a greeting / a question about you) and
/// [`RouteClass::Explain`] (read-only Q&A) resolve nothing durable — they are the
/// firmware's "Light" tier (no knowledge/memory retrieval), so they also get no
/// active extraction (no fork, no token cost on a chat reply). Everything else
/// (a QuickEdit / a Debug / a Build) acts on the workspace and may resolve a path,
/// a command, or a constraint — exactly what the store exists to remember.
#[must_use]
pub(crate) fn route_warrants_extraction(route: &RoutePlan) -> bool {
    !matches!(route.class, RouteClass::Chat | RouteClass::Explain)
}

/// The deterministic throttle: given the running count of WORK turns observed so
/// far (1-based), decide whether to run the active extraction on THIS one.
///
/// Fires on turn 1 (so even a single-step build populates the store) and then on
/// every [`EXTRACT_EVERY_N_WORK_TURNS`]-th turn (3, 6, 9, …). A `0` count never
/// fires. This bounds the extraction frequency — a 9-step build runs it 4 times
/// (1, 3, 6, 9), not 9 — while guaranteeing at least one extraction per build.
#[must_use]
pub(crate) fn should_extract(work_turn_count: usize) -> bool {
    work_turn_count != 0
        && (work_turn_count == 1
            || (EXTRACT_EVERY_N_WORK_TURNS != 0
                && work_turn_count.is_multiple_of(EXTRACT_EVERY_N_WORK_TURNS)))
}

/// The extraction prompt the borrowed brain answers on the read-only fork. It runs
/// in the fork's inherited context (claude/codex fork the live thread, so the fork
/// can see what THIS turn did; opencode's fresh fork still answers from the shared
/// blackboard), so it asks for the durable facts the turn just established as
/// `key: value` lines — or `none`. Deliberately NOT behind the maker-checker
/// firewall: unlike a critic, this consult WANTS to see the turn's work.
#[must_use]
fn extraction_directive() -> String {
    "You are wrapping up a work turn on a software project. Record the DURABLE \
     project facts this turn ESTABLISHED — the things a teammate joining later \
     must not have to re-discover. Include ONLY stable, reusable facts: resolved \
     tool/binary paths, the exact build / run / test commands, environment \
     constraints (required language/runtime versions, ports, env vars), and key \
     architecture or product decisions that are now settled.\n\n\
     Output ONE fact per line as `key: value` — a short stable key, a colon, then \
     the value. For example:\n\
     build: pnpm -w build\n\
     test: pnpm -w test\n\
     api_port: 8787\n\
     jdk: /usr/lib/jvm/jdk-17\n\n\
     Do NOT include transient state, this turn's narration, todo items, or anything \
     you are not confident is stable and reusable. If this turn established no new \
     durable fact, reply with exactly: none"
        .to_string()
}

/// Parse the brain's free-form extraction reply into durable [`Fact`]s.
///
/// Tolerant by design — the reply may carry bullets, markdown, code fences, a
/// header line, or stray prose around the lines. Each line is stripped of a
/// leading list marker (dash / star / bullet / an ordered "n." or "n)"), split on
/// its FIRST colon (ASCII `:` or full-width `：` so a Chinese reply parses too), and
/// its key + value are de-fenced of surrounding markdown (`` ` `` / `*`). A line
/// with no colon, an empty key/value, or a bare `none` token on EITHER side (key
/// OR value) is skipped — so a sentence, a header (`facts:` with an empty value),
/// a `none` reply, AND a "no fact established" line like `node: none` all yield
/// nothing. The result is bounded to [`MAX_FACTS_PER_EXTRACTION`]; further
/// dedup-by-key plus field truncation happen in
/// [`crate::project_facts::record_facts`].
#[must_use]
pub(crate) fn parse_facts(reply: &str) -> Vec<Fact> {
    let mut out: Vec<Fact> = Vec::new();
    for raw in reply.lines() {
        if out.len() >= MAX_FACTS_PER_EXTRACTION {
            break;
        }
        let line = strip_list_marker(raw.trim());
        if line.is_empty() || is_none_token(line) {
            continue;
        }
        // Split on the first ASCII or full-width colon.
        let Some(idx) = line.find([':', '：']) else {
            continue; // no `key: value` shape → not a fact line
        };
        let key = clean_field(&line[..idx]);
        // Step past the (possibly multi-byte) colon char.
        let after = line[idx..]
            .char_indices()
            .nth(1)
            .map_or("", |(off, _)| &line[idx + off..]);
        let value = clean_field(after);
        // Skip an empty key/value, OR a `none` on EITHER side: a `none: …` header
        // and a `node: none` "no fact established" line are both no-ops.
        if key.is_empty() || value.is_empty() || is_none_token(&key) || is_none_token(&value) {
            continue;
        }
        out.push(Fact::new(key, value, None::<String>));
    }
    out
}

/// Parse `reply` and RECORD the durable facts to the store at `root`, returning how
/// many were applied. The thin parse→record seam that the active backstop and the
/// tests share: a `none` / empty / shapeless reply records nothing (`0`), so the
/// file is never created from a no-op. Fail-open via
/// [`crate::project_facts::record_facts`] (a write error is swallowed → still `0`).
pub(crate) fn record_from_reply(root: &Path, reply: &str) -> usize {
    let facts = parse_facts(reply);
    // STALENESS SWEEP first: tombstone any stored LIVE fact this run's observations
    // clearly CONTRADICT (a changed value for the same key) or that has gone dead (a
    // `path` fact whose absolute target no longer exists), so a rotten fact stops
    // being recalled. Runs even on a `none` reply (an empty `facts` still lets the
    // dead-path signal fire); an empty store is a cheap no-op. Non-destructive (the
    // row is kept on disk, just flagged), bounded, deterministic, fully fail-open —
    // then the fresh observation is recorded, superseding any same-key tombstone.
    let _ = project_facts::mark_stale_facts(root, &facts);
    if facts.is_empty() {
        return 0;
    }
    project_facts::record_facts(root, &facts)
}

/// The active recording backstop: AFTER a meaningful work turn, extract this turn's
/// durable facts on a read-only fork and persist them to the store, so
/// `.umadev/memory/facts.jsonl` reliably populates without depending on the base
/// writing it. Returns how many facts were recorded (`0` on any skip / failure).
///
/// Two cheap deterministic gates run BEFORE any fork (so a chat turn or a throttled
/// turn spends zero tokens): `route` must be a work turn ([`route_warrants_extraction`];
/// `None` is treated as work — the legacy no-route build path always reaches here only
/// after claiming code changes), and the throttle ([`should_extract`]) must fire for
/// `work_turn_count`. Only then does it fork the SAME read-only seam the critics use
/// ([`crate::continuous::fork_with_timeout`] + [`crate::continuous::ForkConsult`]),
/// run ONE bounded judge turn, parse the `key: value` lines, and record them.
///
/// **Fail-open at every step:** a failed/wedged fork, an offline brain, a timeout, a
/// `none`/empty/unparseable reply, or an unwritable store all yield `0` — never an
/// error, never a panic, never a blocked turn.
pub(crate) async fn maybe_extract_facts(
    session: &mut dyn BaseSession,
    root: &Path,
    route: Option<&RoutePlan>,
    work_turn_count: usize,
    events: &Arc<dyn EventSink>,
) -> usize {
    // Gate 1 — work-class only: pure chat / explain never establish durable facts,
    // so they never fork (no token cost on a chat reply).
    if route.is_some_and(|r| !route_warrants_extraction(r)) {
        return 0;
    }
    // Gate 2 — throttle: only run on a bounded subset of work turns.
    if !should_extract(work_turn_count) {
        return 0;
    }

    // Fork a read-only session (bounded handshake) and ask the brain to enumerate
    // this turn's durable facts — the EXACT fork→consult mechanism the critic team
    // and the router reuse. Fail-open: a fork that didn't open routes `judge_text`
    // to `None`, and we record nothing.
    let fork = crate::continuous::fork_with_timeout(session).await;
    let consult = crate::continuous::ForkConsult::new(fork);
    let reply = consult
        .judge_text("fact-extract", extraction_directive())
        .await;
    consult.end().await;

    let Some(reply) = reply else {
        return 0;
    };
    let recorded = record_from_reply(root, &reply);
    if recorded > 0 {
        // Surface the backstop so the user can SEE the store populate (the whole
        // point — the file used to silently never appear). Advisory note only.
        events.emit(EngineEvent::Note(format!(
            "memory · recorded {recorded} durable project fact(s) → {}",
            project_facts::FACTS_REL_PATH
        )));
    }
    recorded
}

/// Strip a single leading unordered/ordered list marker (`- `, `* `, `• `, `1. `,
/// `2) `) from a trimmed line so a bulleted reply parses. Returns the remainder,
/// re-trimmed. A line with no marker is returned unchanged.
fn strip_list_marker(line: &str) -> &str {
    for m in ["- ", "* ", "• ", "+ "] {
        if let Some(rest) = line.strip_prefix(m) {
            return rest.trim_start();
        }
    }
    // Ordered marker: leading digits then `.`/`)` then space (e.g. "1. " / "2) ").
    let digits: String = line.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let rest = &line[digits.len()..];
        if let Some(after) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return after.trim_start();
        }
    }
    line
}

/// De-fence one key/value field: trim, then strip surrounding markdown emphasis /
/// inline-code punctuation (`` ` ``, `*`, `"`) so `` `build` `` → `build` and
/// `**api_port**` → `api_port`. Conservative — only strips matched wrapping chars.
fn clean_field(field: &str) -> String {
    let mut s = field.trim();
    // Peel balanced wrappers a couple of layers deep (e.g. `**`token`**`).
    for _ in 0..3 {
        let trimmed = s
            .strip_prefix("**")
            .and_then(|t| t.strip_suffix("**"))
            .or_else(|| s.strip_prefix('`').and_then(|t| t.strip_suffix('`')))
            .or_else(|| s.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
            .or_else(|| s.strip_prefix('*').and_then(|t| t.strip_suffix('*')))
            .map(str::trim);
        match trimmed {
            Some(t) if t.len() < s.len() => s = t,
            _ => break,
        }
    }
    s.to_string()
}

/// Whether `s` is a bare "none" token (ignoring case + surrounding punctuation /
/// parens), so a `none` / `None.` / `(none)` reply records nothing.
fn is_none_token(s: &str) -> bool {
    let core: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    core == "none"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::critics::Seat;
    use crate::planner::TaskKind;
    use crate::router::{Budget, Depth};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use umadev_runtime::{ApprovalDecision, SessionError, SessionEvent, TurnStatus};

    // ── Minimal scripted fake BaseSession (the extraction needs to fork) ───────
    //
    // `MainFake::fork()` either fails (fork_fails → ForkUnsupported, the fail-open
    // path) or hands back a `ScriptedFork` that emits `reply` then a clean
    // TurnDone — so a test drives the whole extract→parse→record path with no real
    // base. `forks` counts opened forks so a test can prove a SKIP never forked.

    struct ScriptedFork {
        events: VecDeque<SessionEvent>,
    }
    #[async_trait::async_trait]
    impl BaseSession for ScriptedFork {
        async fn send_turn(&mut self, _d: String) -> Result<(), SessionError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.events.pop_front()
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

    struct MainFake {
        reply: String,
        fork_fails: bool,
        forks: Arc<Mutex<usize>>,
    }
    impl MainFake {
        fn replying(reply: &str) -> Self {
            Self {
                reply: reply.to_string(),
                fork_fails: false,
                forks: Arc::new(Mutex::new(0)),
            }
        }
        fn fork_failing() -> Self {
            Self {
                reply: String::new(),
                fork_fails: true,
                forks: Arc::new(Mutex::new(0)),
            }
        }
        fn forks_handle(&self) -> Arc<Mutex<usize>> {
            Arc::clone(&self.forks)
        }
    }
    #[async_trait::async_trait]
    impl BaseSession for MainFake {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            *self.forks.lock().unwrap() += 1;
            if self.fork_fails {
                return Err(SessionError::ForkUnsupported("scripted".into()));
            }
            Ok(Box::new(ScriptedFork {
                events: VecDeque::from(vec![
                    SessionEvent::TextDelta(self.reply.clone()),
                    SessionEvent::TurnDone {
                        status: TurnStatus::Completed,
                        usage: None,
                    },
                ]),
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

    fn build_route() -> RoutePlan {
        RoutePlan {
            class: RouteClass::Build,
            kind: TaskKind::Greenfield,
            depth: Depth::Standard,
            team: vec![Seat::BackendEngineer],
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(RouteClass::Build, Depth::Standard),
            confidence: 0.6,
        }
    }
    fn chat_route() -> RoutePlan {
        RoutePlan {
            class: RouteClass::Chat,
            kind: TaskKind::Light,
            depth: Depth::Fast,
            team: Vec::new(),
            scope: Vec::new(),
            needs_clarify: None,
            est_budget: Budget::for_route(RouteClass::Chat, Depth::Fast),
            confidence: 0.6,
        }
    }
    fn sink() -> Arc<dyn EventSink> {
        Arc::new(crate::events::NullSink)
    }

    // ── Pure parser ───────────────────────────────────────────────────────────

    #[test]
    fn parse_extracts_key_value_lines_tolerating_markup() {
        let reply = "Here are the durable facts:\n\
                     - build: `pnpm -w build`\n\
                     * api_port: 8787\n\
                     **jdk**: /usr/lib/jvm/jdk-17\n\
                     a sentence with no colon at all\n\
                     1. test: pnpm -w test";
        let facts = parse_facts(reply);
        let got: std::collections::HashMap<_, _> = facts
            .iter()
            .map(|f| (f.key.as_str(), f.value.as_str()))
            .collect();
        assert_eq!(got.get("build"), Some(&"pnpm -w build"));
        assert_eq!(got.get("api_port"), Some(&"8787"));
        assert_eq!(got.get("jdk"), Some(&"/usr/lib/jvm/jdk-17"));
        assert_eq!(got.get("test"), Some(&"pnpm -w test"));
        // The header ("…facts:" → empty value) and the prose line are NOT facts.
        assert!(!got.contains_key("Here are the durable facts"));
        assert_eq!(facts.len(), 4, "only the 4 key:value lines: {facts:?}");
    }

    #[test]
    fn parse_handles_a_full_width_colon() {
        // A Chinese reply may use a full-width colon — it must still parse.
        let facts = parse_facts("数据库端口：5432");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "数据库端口");
        assert_eq!(facts[0].value, "5432");
    }

    #[test]
    fn parse_treats_none_as_no_facts() {
        for r in ["none", "None.", "  (none)  ", "NONE", ""] {
            assert!(parse_facts(r).is_empty(), "{r:?} → no facts");
        }
    }

    #[test]
    fn parse_skips_a_none_valued_line() {
        // Low: a `key: none` line means "no fact established" for that key — it must
        // NOT record a fact with the literal value "none". A real fact alongside it
        // still parses.
        let facts = parse_facts("node: none\napi_port: 8787\njdk: None.");
        let keys: Vec<&str> = facts.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["api_port"], "only the real fact survives: {facts:?}");
        assert!(
            !facts.iter().any(|f| f.value.eq_ignore_ascii_case("none")),
            "no fact may carry a bare 'none' value: {facts:?}"
        );
    }

    #[test]
    fn parse_is_bounded_per_extraction() {
        let mut big = String::new();
        for i in 0..(MAX_FACTS_PER_EXTRACTION + 20) {
            big.push_str(&format!("k{i}: v{i}\n"));
        }
        assert_eq!(parse_facts(&big).len(), MAX_FACTS_PER_EXTRACTION);
    }

    // ── parse → record ────────────────────────────────────────────────────────

    #[test]
    fn record_from_reply_populates_the_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let n = record_from_reply(tmp.path(), "build: cargo build\napi_port: 8080");
        assert_eq!(n, 2);
        // The file the user expected now exists + holds the facts.
        assert!(tmp.path().join(project_facts::FACTS_REL_PATH).exists());
        let facts = project_facts::load_facts(tmp.path());
        assert!(facts
            .iter()
            .any(|f| f.key == "build" && f.value == "cargo build"));
        assert!(facts
            .iter()
            .any(|f| f.key == "api_port" && f.value == "8080"));
    }

    #[test]
    fn record_from_a_none_reply_writes_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(record_from_reply(tmp.path(), "none"), 0);
        // No no-op file: the store was never created from an empty extraction.
        assert!(!tmp.path().join(project_facts::FACTS_REL_PATH).exists());
        assert!(project_facts::load_facts(tmp.path()).is_empty());
    }

    // ── Throttle + work-class gates ────────────────────────────────────────────

    #[test]
    fn throttle_fires_first_then_every_nth_and_bounds_frequency() {
        assert!(
            should_extract(1),
            "first work turn fires (a 1-step build populates)"
        );
        assert!(!should_extract(2));
        assert!(should_extract(3));
        assert!(!should_extract(4));
        assert!(!should_extract(5));
        assert!(should_extract(6));
        assert!(!should_extract(0), "a 0 count never fires");
        // Over 9 work turns it fires a bounded subset (1,3,6,9), not all 9.
        let fired = (1..=9).filter(|n| should_extract(*n)).count();
        assert!(fired < 9 && fired == 4, "throttled to {fired}/9 turns");
    }

    #[test]
    fn only_work_routes_warrant_extraction() {
        assert!(route_warrants_extraction(&build_route()));
        assert!(!route_warrants_extraction(&chat_route()));
        let mut explain = chat_route();
        explain.class = RouteClass::Explain;
        assert!(!route_warrants_extraction(&explain));
    }

    // ── Orchestrator (fork → extract → record) ─────────────────────────────────

    #[tokio::test]
    async fn active_extraction_on_a_work_turn_populates_the_store() {
        // The whole point: a work turn extracts facts ITSELF and the file appears,
        // without the base ever voluntarily writing it.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut session = MainFake::replying("build: pnpm -w build\napi_port: 8787");
        let route = build_route();
        let n = maybe_extract_facts(&mut session, tmp.path(), Some(&route), 1, &sink()).await;
        assert_eq!(n, 2, "both facts recorded");
        assert!(tmp.path().join(project_facts::FACTS_REL_PATH).exists());
        let facts = project_facts::load_facts(tmp.path());
        assert!(facts.iter().any(|f| f.key == "build"));
        assert!(facts.iter().any(|f| f.key == "api_port"));
    }

    #[tokio::test]
    async fn a_none_reply_records_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut session = MainFake::replying("none");
        let route = build_route();
        let n = maybe_extract_facts(&mut session, tmp.path(), Some(&route), 1, &sink()).await;
        assert_eq!(n, 0);
        assert!(!tmp.path().join(project_facts::FACTS_REL_PATH).exists());
    }

    #[tokio::test]
    async fn a_pure_chat_turn_is_skipped_without_forking() {
        // A chat turn must NOT fork (no token cost) and must record nothing — even
        // though the (unused) reply would have parsed.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut session = MainFake::replying("build: pnpm -w build");
        let forks = session.forks_handle();
        let route = chat_route();
        let n = maybe_extract_facts(&mut session, tmp.path(), Some(&route), 1, &sink()).await;
        assert_eq!(n, 0, "chat extracts nothing");
        assert_eq!(*forks.lock().unwrap(), 0, "chat never forks");
        assert!(!tmp.path().join(project_facts::FACTS_REL_PATH).exists());
    }

    #[tokio::test]
    async fn a_throttled_off_turn_is_skipped_without_forking() {
        // Work route but a non-firing throttle count → no fork, no record.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut session = MainFake::replying("build: pnpm -w build");
        let forks = session.forks_handle();
        let route = build_route();
        let n = maybe_extract_facts(&mut session, tmp.path(), Some(&route), 2, &sink()).await;
        assert_eq!(n, 0);
        assert_eq!(*forks.lock().unwrap(), 0, "throttled-off turn never forks");
    }

    #[tokio::test]
    async fn fail_open_when_the_fork_fails() {
        // A fork that can't open (offline / unsupported) must degrade to 0 facts,
        // never an error/panic — the turn is unaffected.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut session = MainFake::fork_failing();
        let route = build_route();
        let n = maybe_extract_facts(&mut session, tmp.path(), Some(&route), 1, &sink()).await;
        assert_eq!(n, 0, "fork failure → nothing recorded, no panic");
        assert!(!tmp.path().join(project_facts::FACTS_REL_PATH).exists());
    }
}
