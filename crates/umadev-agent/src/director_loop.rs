//! The director build loop — the USB / smart-hardware model of
//! `docs/AGENT_WIELDS_BASE_ARCHITECTURE.md` (simplified: NO marker protocol).
//!
//! ## The model: UmaDev is firmware; the base is the brain + hands
//!
//! UmaDev is a smart device with its own firmware — a senior team-director
//! identity, engineering taste, accumulated knowledge, governance, and memory —
//! but **no compute of its own**. Plugged into a base (claude / codex / opencode)
//! over the continuous session, it **borrows the base's intelligence and hands**
//! to get work done, the way a smart peripheral borrows a host computer's CPU and
//! storage. The firmware is injected into the base (via the directive + system
//! prompt the caller built — `experts::director_build_directive` /
//! `experts::director_with_team_tools`); the base then thinks, plans, and writes
//! files with its OWN internal agentic tool loop.
//!
//! **The key insight that retired the old marker protocol:** the base is already a
//! whole brain. Once UmaDev's firmware is injected, the base ITSELF plays PM /
//! architect / frontend / QA internally and builds the goal end to end. It does
//! **not** need to emit `<<<umadev:summon …>>>` markers for UmaDev to "summon a
//! team" from the outside — real-machine testing showed the base writes good,
//! multi-role code with ZERO markers, because the team lives inside the base's
//! head, steered by the firmware. So this loop no longer asks the base to speak a
//! scheduling protocol, and no longer parses one.
//!
//! ## The boundary: UmaDev grows NO "operating" machinery of its own
//!
//! The base is a complete Agent already — its model is the brain, its CLI tools are
//! the body that builds code, writes files, runs commands, runs tests, and fixes
//! bugs. UmaDev is the EXTERNAL agent that plugs into that body and shares that
//! brain. So **UmaDev never grows its own build/write/run/test/fix capability** —
//! all of that work is the base's body using the base's own tools. The only two
//! tiny things UmaDev does for itself (firmware business, not "operating"):
//!
//! - **Governance** — the background safety net riding on the base's file writes
//!   (the existing PreToolUse hook). Untouched here.
//! - **A read-only honesty check** — when the base says "built it", UmaDev reads the
//!   disk to confirm real code actually exists ([`crate::acceptance::source_files`]
//!   via [`crate::director::verify`] / [`VerifyKind::SourcePresent`]). Tiny,
//!   deterministic, read-only — it just stops a hallucinated "done".
//!
//! Everything else — building, and FIXING what QC surfaces — the base's body does
//! with its own tools, steered by a fix directive UmaDev feeds back. UmaDev reads
//! objective facts and judges; it does not operate.
//!
//! ## The loop: end-to-end build → UmaDev honesty/QC read → bounded feedback-fix
//!
//! 1. Drive the base end to end on the goal (one firmware-injected turn — its own
//!    agentic tool loop runs PM→…→QA internally and writes real files).
//! 2. When the turn settles, **UmaDev reads reality ITSELF** (it does not wait for
//!    the base to ask, and it does not operate):
//!    - **honesty hard floor (deterministic, read-only)** — did real source files
//!      actually land ([`crate::acceptance::source_files`])? This is UmaDev's own
//!      tiny check.
//!    - **optional fork review (read-only, borrows the brain)** — for a build that
//!      produced real code, fork the review team on isolated read-only sessions and
//!      collect blocking findings ([`crate::director::review`]). UmaDev judges with
//!      the borrowed brain; it writes nothing.
//! 3. If QC found blocking problems, fold them into ONE fix directive and feed it
//!    back to the base over the same session (the USB channel) — and the directive
//!    tells the base's body to **run its own build/test and fix the cause with its
//!    own tools**. Then re-read. **Bounded** by [`MAX_QC_ROUNDS`].
//! 4. Clean (or no code claimed — a chat/plan answer) → done; report honestly.
//!
//! Note on build/test: UmaDev does NOT run the build itself as its gate — the base's
//! body runs build/test (it has the tools), and the fix directive explicitly asks it
//! to. The optional [`VerifyKind::BuildTest`] read is retained only as a cheap
//! reuse of an EXISTING reader to surface an objective failure fact when a manifest
//! is present; it is positioned as "read a fact", never as "UmaDev operates".
//!
//! ## Floor preserved (every invariant still holds)
//!
//! 1. **Single-writer.** Only the MAIN base turn mutates the workspace, under the
//!    run-lock the caller holds. UmaDev's checks are read-only: the source floor
//!    reads disk; the QC review runs on isolated read-only forks
//!    ([`crate::director::review`]). Nothing UmaDev does writes the workspace.
//! 2. **Objective floor untouched.** The source-present floor is a deterministic,
//!    read-only reality check; review verdicts stay advisory (they only seed a fix
//!    directive the base acts on, they never themselves decide "done"). The caller's
//!    source-present hard-gate still runs at the boundary, unchanged.
//! 3. **Governance + audit.** Every base turn (the first build and every fix pass)
//!    drives the SAME governed/audited session; the PreToolUse hook still fires
//!    under every write.
//! 4. **No new endpoint.** Every QC review reads over the SAME borrowed brain + its
//!    `fork()`; no extra model endpoint, no API key.
//! 5. **Fail-open.** A QC read that can't run degrades to "no problem found" (a
//!    review that can't fork accepts), never a false failure that wedges the loop. A
//!    dead session is an honest `Failed`, never a panic.
//! 6. **Reversible.** This loop is the DEFAULT `/run` path; the legacy fixed
//!    pipeline (`UMADEV_LEGACY_PIPELINE=1`) is untouched.

use std::sync::Arc;

use umadev_runtime::{ApprovalDecision, BaseSession, SessionEvent, StreamEvent, TurnStatus};

use crate::director::{self, ReviewResult, VerifyKind, VerifyResult};
use crate::events::{EngineEvent, EventSink};
use crate::runner::RunOptions;
use crate::trust::requires_confirmation;

/// The hard ceiling on auto-QC feedback-fix rounds in one `/run`. One round is: the
/// base builds (or fixes) end to end, then UmaDev runs its objective QC pass. A
/// clean pass ends the loop immediately, so a simple goal that builds correctly the
/// first time spends ZERO fix rounds. The cap only bounds a goal that keeps failing
/// QC — after it, the loop ends and the caller's source-present hard-gate has the
/// final say. Mirrors the proven bounded-rework shape (`continuous::MAX_REWORK_ROUNDS`)
/// at the build level: small + decisive, not an open-ended grind.
const MAX_QC_ROUNDS: usize = 3;

/// How the director loop settled. Mirrors the caller's existing director outcome but
/// lives in the agent crate so both the CLI and the TUI share ONE loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectorLoopOutcome {
    /// The build finished — the base built end to end and UmaDev's auto-QC either
    /// passed or exhausted its bounded fix budget. The caller then runs the
    /// objective source-present hard-gate to verify reality.
    Done {
        /// The base's final assistant text — the caller reads it for a "claimed a
        /// build" check against the real source files.
        reply: String,
    },
    /// The session died / a turn failed — an honest hard stop, never disguised as
    /// success. Carries a machine-true reason.
    Failed(String),
}

/// Drive an explicit `/run` (full product build) through the **director build loop**
/// — the USB-model engine. ONE live [`BaseSession`] is the director's brain; the
/// firmware (team identity + craft + knowledge) is already injected by the caller
/// into `first_directive` (and, on the TUI path, the system prompt). The base builds
/// the goal end to end with its OWN internal agentic loop; then UmaDev runs an
/// objective QC pass ITSELF ([`run_auto_qc`]) and, if QC found blocking problems,
/// feeds ONE fix directive back over the same session for another pass — bounded by
/// [`MAX_QC_ROUNDS`].
///
/// `first_directive` is the goal framing the caller already built (e.g.
/// [`crate::experts::director_build_directive`]). The caller owns the session
/// lifetime (and the run-lock) and `end()`s it after this returns.
///
/// Floor preserved (see the module docs): single-writer, governance + audit,
/// advisory review, objective verify, fail-open, no endpoint. The loop never blocks
/// the host — every failure mode degrades to a graceful settle.
pub async fn drive_director_loop(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    first_directive: String,
) -> DirectorLoopOutcome {
    let mut next_directive = first_directive;
    let mut last_reply = String::new();

    for round in 0..MAX_QC_ROUNDS {
        // 1. Drive ONE end-to-end base turn (build, or fix-the-QC-findings). The
        //    base runs its own agentic tool loop (PM→…→QA internally) and writes
        //    real files under the run-lock the caller holds (single-writer).
        let turn = match drive_one_turn(session, options, events, next_directive).await {
            Ok(t) => t,
            Err(reason) => return DirectorLoopOutcome::Failed(reason),
        };
        last_reply = turn.text.clone();

        // 2. If the base didn't claim it built/changed code (a chat / plan / "I read
        //    it" answer), there is nothing to QC — settle. This keeps a simple goal
        //    that the base just answered directly from being forced through QC.
        if !crate::gates::claims_code_changes(&turn.text) {
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 3. UmaDev runs its OWN objective QC pass — hard floor + verify + optional
        //    fork review. NOTHING here is the base summoning a team; it is UmaDev
        //    inspecting reality over the borrowed brain.
        let qc = run_auto_qc(session, options, events).await;

        // 4. Clean QC → the build is genuinely done. Settle and report honestly.
        if qc.is_clean() {
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 5. QC found blocking problems. Out of fix budget → settle (the caller's
        //    source-present hard-gate is the objective backstop).
        if round + 1 >= MAX_QC_ROUNDS {
            events.emit(EngineEvent::Note(
                "team · auto-QC reached its fix-round budget — settling (objective hard-gate decides reality)"
                    .to_string(),
            ));
            return DirectorLoopOutcome::Done { reply: last_reply };
        }

        // 6. Fold the QC findings into ONE fix directive and feed it back over the
        //    USB channel for another build pass → re-QC.
        next_directive = qc.fix_directive();
    }

    DirectorLoopOutcome::Done { reply: last_reply }
}

/// One base turn's observable result.
struct TurnResult {
    /// The accumulated assistant text. The caller reads it for the "claimed a build"
    /// hard-gate; this loop reads it to decide whether QC is even warranted.
    text: String,
}

/// Send one directive and pump the base's event stream to its `TurnDone`, forwarding
/// tool calls + text to the live sink (the SAME `WorkerStream` render path the
/// pipeline uses), answering approvals via the always-on irreversible floor, and
/// accumulating the assistant text. Returns the turn's text, or `Err` with a
/// machine-true reason on a failed / dead turn (fail-open: the caller maps it to a
/// hard stop, never a panic).
async fn drive_one_turn(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    directive: String,
) -> Result<TurnResult, String> {
    if let Err(e) = session.send_turn(directive).await {
        return Err(format!("session send: {e}"));
    }
    let mut text = String::new();
    loop {
        let Some(ev) = session.next_event().await else {
            // `None` = the session ended (process dead / EOF). Per the BaseSession
            // contract, treat as a failed turn — fail-open, no panic.
            return Err("base session ended mid-turn".to_string());
        };
        match ev {
            SessionEvent::TextDelta(delta) => {
                text.push_str(&delta);
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::Text { delta },
                });
            }
            SessionEvent::ToolCall { name, input } => {
                // Surface what the base actually DID (the source of truth). The
                // governance hook governs the write itself in real time; here we
                // render the tool row for live progress.
                let detail = tool_call_target(&input);
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolUse { name, detail },
                });
            }
            SessionEvent::ToolResult { ok, summary } => {
                events.emit(EngineEvent::WorkerStream {
                    event: StreamEvent::ToolResult { ok, summary },
                });
            }
            SessionEvent::NeedApproval {
                req_id,
                action,
                target,
            } => {
                // Always-on irreversible floor: deny an irreversible action even
                // headless (the same floor the `auto` tier can't skip), allow the
                // rest so a headless build isn't wedged waiting on a human.
                let decision = if requires_confirmation(options.mode, &action, &target) {
                    events.emit(EngineEvent::Note(umadev_i18n::tlf(
                        "continuous.dangerous_action_denied",
                        &[&action, &target],
                    )));
                    ApprovalDecision::Deny
                } else {
                    ApprovalDecision::Allow
                };
                if let Err(e) = session.respond(&req_id, decision).await {
                    return Err(format!("session respond: {e}"));
                }
            }
            SessionEvent::TurnDone { status } => match status {
                // Completed / Truncated → accept the turn (the deterministic floor
                // downstream is the real stop signal; forcing a fail here would
                // hard-stop a build that may have produced usable output).
                TurnStatus::Completed | TurnStatus::Truncated => {
                    return Ok(TurnResult { text });
                }
                TurnStatus::Interrupted => return Err("director turn interrupted".to_string()),
                TurnStatus::Failed(reason) => return Err(reason),
            },
        }
    }
}

/// Best-effort human-readable target of a base tool call (a file path / command)
/// for the live tool row — fail-open to an empty string on any unexpected shape.
fn tool_call_target(input: &serde_json::Value) -> String {
    for key in ["file_path", "path", "command", "url", "pattern"] {
        if let Some(s) = input.get(key).and_then(serde_json::Value::as_str) {
            return s.to_string();
        }
    }
    String::new()
}

// ───────────────────────────────────────────────────────────────────────────
// Auto-QC — UmaDev's objective quality pass (NOT the base summoning a team)
// ───────────────────────────────────────────────────────────────────────────

/// What one auto-QC pass found. Empty `blocking` = clean (the build is genuinely
/// done). Non-empty = the factual problems UmaDev folds into ONE fix directive for
/// the base. Built fail-open: any QC step that can't run contributes nothing (a
/// neutral skip), never a false blocking finding.
#[derive(Debug, Clone, Default)]
struct QcReport {
    /// The deduped, source-tagged union of blocking problems (e.g. `verify build:
    /// FAILED …`, `[security] no input validation`). Empty = clean.
    blocking: Vec<String>,
}

impl QcReport {
    /// Whether the build passed QC clean (no blocking problem found).
    fn is_clean(&self) -> bool {
        self.blocking.is_empty()
    }

    /// Fold the QC findings into ONE fix directive fed back to the base over the
    /// same session. The BASE'S BODY does the fixing (and the build/test) with its
    /// own tools — UmaDev only hands it the facts and asks it to act. Lean +
    /// command-style so the base fixes rather than narrates; it already holds the
    /// full build context in this one continuous session, so no role re-priming.
    fn fix_directive(&self) -> String {
        let mut body = String::new();
        for b in &self.blocking {
            body.push_str("- ");
            body.push_str(b);
            body.push('\n');
        }
        format!(
            "An objective check of what you just built surfaced problems that must be \
             fixed (these are real facts read from disk / review, not your memory):\n\
             {body}\nFix the cause of each one yourself with your tools — edit/create \
             the real files — then RUN the project's own build and tests to confirm \
             they pass. When it is genuinely clean, end your turn and report honestly \
             what you fixed."
        )
    }
}

/// Run UmaDev's READ-ONLY QC pass over what the base just built — UmaDev judges, it
/// does not operate. The base's body did the building; here UmaDev only reads
/// reality. Checks, cheapest-first, each fail-open:
///
/// 1. **Honesty hard floor (UmaDev's own deterministic, read-only check)** — real
///    source files actually landed ([`crate::director::verify`] with
///    [`VerifyKind::SourcePresent`], which just reads disk). Zero source after a
///    claimed build is the decisive blocking finding (nothing was built) and short-
///    circuits the rest.
/// 2. **Build/test FACT read (optional, reuse of an existing reader)** — when a
///    project manifest is present, [`VerifyKind::BuildTest`] surfaces an objective
///    failure fact. This is positioned as reading a fact, NOT UmaDev's gate: the fix
///    directive asks the BASE to run its own build/test. A skipped check is neutral.
/// 3. **Optional fork review (read-only, borrows the brain)** — only when real code
///    exists, fork the review team on read-only sessions and collect blocking
///    findings ([`crate::director::review`]). Advisory: findings only seed the fix
///    directive the base acts on.
///
/// Single-writer preserved: every step is read-only (disk read / build-test read /
/// isolated read-only forks) — NOTHING here writes the workspace.
async fn run_auto_qc(
    session: &mut dyn BaseSession,
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
) -> QcReport {
    events.emit(EngineEvent::Note("team · honesty + QC read".to_string()));
    let mut blocking: Vec<String> = Vec::new();

    // 1. Honesty hard floor (UmaDev's own read-only check): did real source actually
    //    land? A claimed build with zero source is the decisive blocking finding —
    //    feed it back so the base's body actually writes the code.
    let src = director::verify(options, events, VerifyKind::SourcePresent).await;
    if src.available && !src.passed {
        blocking.push(format!(
            "source-present: FAILED — {} (the build claimed done but no real source \
             files exist on disk; actually create the code with your tools)",
            src.evidence.first().cloned().unwrap_or_default()
        ));
        // No source means nothing to build/test or review — return now with the
        // decisive finding rather than reading over an empty tree.
        return QcReport { blocking };
    }

    // 2. Build/test FACT read (optional, when a manifest is present). UmaDev reads
    //    the objective result; the FIX is the base's job (the fix directive tells it
    //    to run its own build/test). A skipped check is neutral (fail-open).
    let bt = director::verify(options, events, VerifyKind::BuildTest).await;
    if let Some(line) = build_test_blocking(&bt) {
        blocking.push(line);
    }

    // 3. Optional fork review (UmaDev's read-only QC over read-only forks). The team
    //    scales to the task, so a lean goal convenes no team and this contributes
    //    nothing. Advisory — the base's body acts on whatever it surfaces.
    let review = director::review(
        session,
        options,
        events,
        crate::continuous::ReviewKind::Quality,
    )
    .await;
    for finding in review_blocking(&review) {
        blocking.push(finding);
    }

    QcReport { blocking }
}

/// Map a [`VerifyResult`] from a build/test check to a blocking line, or `None` when
/// it passed / was skipped (an unavailable check is neutral — fail-open).
fn build_test_blocking(r: &VerifyResult) -> Option<String> {
    if !r.available || r.passed {
        return None;
    }
    let detail = if r.evidence.is_empty() {
        String::new()
    } else {
        format!(" — {}", r.evidence.join("; "))
    };
    Some(format!("verify build-test: FAILED{detail}"))
}

/// Pull the blocking findings out of a [`ReviewResult`] (already seat-tagged +
/// deduped by the review team). Empty when the team accepted or no team convened.
fn review_blocking(r: &ReviewResult) -> Vec<String> {
    r.blocking.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RecordingSink;
    use crate::trust::TrustMode;
    use umadev_runtime::{SessionError, SessionEvent, TurnStatus};

    fn opts(root: &std::path::Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "做一个登录系统".to_string(),
            slug: "demo".to_string(),
            model: String::new(),
            backend: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: TrustMode::Auto,
            strict_coverage: false,
        }
    }

    fn sink() -> (Arc<dyn EventSink>, RecordingSink) {
        let rec = RecordingSink::default();
        (Arc::new(rec.clone()), rec)
    }

    // ── A scripted fake BaseSession: each `send_turn` loads the next scripted
    // batch of events (a turn). Forks emit a fixed JSON verdict so a QC review gets
    // a verdict. `next_event` drains the current batch. ──
    #[derive(Clone)]
    struct FakeSession {
        /// One event-batch per upcoming MAIN turn, consumed front-to-back.
        turns: std::collections::VecDeque<Vec<SessionEvent>>,
        /// The currently-draining batch.
        current: std::collections::VecDeque<SessionEvent>,
        /// Directives the MAIN session received, in order (asserted by tests).
        sent: Arc<std::sync::Mutex<Vec<String>>>,
        /// Whether `fork()` succeeds.
        can_fork: bool,
        /// JSON a forked judge turn emits.
        fork_reply: String,
        /// `true` once this is a forked (read-only) session.
        is_fork: bool,
    }

    impl FakeSession {
        fn new(turns: Vec<Vec<SessionEvent>>, can_fork: bool, fork_reply: &str) -> Self {
            Self {
                turns: turns.into_iter().collect(),
                current: std::collections::VecDeque::new(),
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                can_fork,
                fork_reply: fork_reply.to_string(),
                is_fork: false,
            }
        }
        fn sent_handle(&self) -> Arc<std::sync::Mutex<Vec<String>>> {
            Arc::clone(&self.sent)
        }
    }

    #[async_trait::async_trait]
    impl BaseSession for FakeSession {
        async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
            if !self.can_fork {
                return Err(SessionError::ForkUnsupported("test".into()));
            }
            let mut f = self.clone();
            f.is_fork = true;
            f.current.clear();
            f.turns.clear();
            Ok(Box::new(f))
        }
        async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
            if self.is_fork {
                // A forked judge turn emits its JSON verdict then ends.
                self.current = [
                    SessionEvent::TextDelta(self.fork_reply.clone()),
                    SessionEvent::TurnDone {
                        status: TurnStatus::Completed,
                    },
                ]
                .into_iter()
                .collect();
                return Ok(());
            }
            self.sent.lock().unwrap().push(directive);
            self.current = self
                .turns
                .pop_front()
                .unwrap_or_else(|| {
                    vec![SessionEvent::TurnDone {
                        status: TurnStatus::Completed,
                    }]
                })
                .into_iter()
                .collect();
            Ok(())
        }
        async fn next_event(&mut self) -> Option<SessionEvent> {
            self.current.pop_front()
        }
        async fn respond(
            &mut self,
            _req_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionError> {
            Ok(())
        }
        async fn interrupt(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
        async fn end(&mut self) -> Result<(), SessionError> {
            Ok(())
        }
    }

    fn text_turn(s: &str) -> Vec<SessionEvent> {
        vec![
            SessionEvent::TextDelta(s.to_string()),
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
            },
        ]
    }

    /// Write a minimal real source file so the source-present floor passes and QC
    /// moves on to build/test + review (instead of stopping at the hard floor).
    fn seed_source(root: &std::path::Path) {
        std::fs::write(root.join("app.ts"), "export const x = 1;").unwrap();
    }

    // ── The USB-model loop: base builds end to end → UmaDev auto-QC → bounded fix ──

    #[tokio::test]
    async fn clean_build_passes_qc_with_no_markers_and_finishes() {
        // The base builds end to end and ends WITHOUT any scheduling marker (the
        // whole point: the team lives in the base's head). With real source on disk
        // and no fork (no review team), auto-QC is clean → done in one base turn.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let turns = vec![text_turn(
            "I created the login form, the route, and the tests — implemented it end \
             to end. All done.",
        )];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        match outcome {
            DirectorLoopOutcome::Done { reply } => assert!(reply.contains("created the login")),
            other @ DirectorLoopOutcome::Failed(_) => panic!("expected Done, got {other:?}"),
        }
        let sent = sent.lock().unwrap();
        // Exactly ONE main directive: the opening build. Clean QC → no fix pass.
        assert_eq!(sent.len(), 1, "clean QC → no feedback-fix turn: {sent:?}");
        assert!(sent[0].contains("GO"), "opening directive sent");
    }

    #[tokio::test]
    async fn qc_finds_no_source_and_feeds_a_fix_directive_back() {
        // The base CLAIMS a build but writes no source. UmaDev's hard-floor QC
        // catches it and feeds a fix directive back over the USB channel; the next
        // base turn writes real source → re-QC clean → done.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // Turn 1 CLAIMS a build (a change verb) but writes no source → the hard-floor
        // QC FAILS and a fix directive is fed back. Turn 2 claims done again (the
        // tree stays empty in this scripted fake, but we only assert the fix
        // directive was injected, which proves the feedback path fired).
        let turns = vec![
            text_turn("Implemented it. (but the fake wrote nothing to disk)"),
            text_turn("Now created app.ts and the tests. Done."),
        ];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        let sent = sent.lock().unwrap();
        // The opening build, then a fix directive carrying the source-present finding.
        assert!(
            sent.iter()
                .any(|d| d.contains("source-present") && d.contains("must be fixed")),
            "the QC finding was fed back as a fix directive: {sent:?}"
        );
    }

    #[tokio::test]
    async fn qc_review_blocking_is_fed_back_as_a_fix_directive() {
        // Real source exists, build/test is skipped (no manifest), but a forked
        // review seat raises a blocking finding → UmaDev folds it into a fix
        // directive over the same session.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let reply = r#"{"accepts": false, "blocking": ["登录失败路径无测试"]}"#;
        let turns = vec![
            text_turn("Created the login form and route. Done."),
            text_turn("Added the failure-path tests. Done."),
        ];
        let mut sess = FakeSession::new(turns, true, reply);
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter()
                .any(|d| d.contains("登录失败路径无测试") && d.contains("must be fixed")),
            "the review blocking finding was fed back as a fix directive: {sent:?}"
        );
    }

    #[tokio::test]
    async fn a_chat_answer_with_no_code_claim_skips_qc_entirely() {
        // A goal the base just answers in prose (no claim of code changes) → no QC,
        // settle after one turn. Keeps a simple ask from being forced through QC.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let turns = vec![text_turn(
            "Here is how I would approach this conceptually, before any code — let me \
             walk through the trade-offs first.",
        )];
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "a non-code answer skips QC → one turn, no re-injection"
        );
    }

    #[tokio::test]
    async fn fix_loop_is_bounded_by_max_qc_rounds() {
        // The base keeps claiming a build but never writes source — QC keeps failing.
        // The loop must STOP at MAX_QC_ROUNDS, never spin forever (bounded), and end
        // gracefully (the caller's hard-gate decides reality).
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // Every turn claims a build (a change verb) but the tree stays empty → the
        // hard-floor QC fails every round, so the loop keeps feeding fix directives
        // until it hits MAX_QC_ROUNDS.
        let turns: Vec<Vec<SessionEvent>> = (0..MAX_QC_ROUNDS + 3)
            .map(|_| text_turn("Implemented it (but still wrote nothing)."))
            .collect();
        let mut sess = FakeSession::new(turns, false, "");
        let sent = sess.sent_handle();
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(matches!(outcome, DirectorLoopOutcome::Done { .. }));
        // Exactly MAX_QC_ROUNDS base build turns were driven, then the loop settled
        // gracefully — the fix loop is BOUNDED, never an open-ended grind.
        assert_eq!(
            sent.lock().unwrap().len(),
            MAX_QC_ROUNDS,
            "the fix loop is bounded by MAX_QC_ROUNDS"
        );
    }

    #[tokio::test]
    async fn dead_session_is_a_failed_outcome_not_a_panic() {
        // A session that ends mid-turn (next_event → None with no TurnDone) is an
        // honest Failed outcome — fail-open, never a panic.
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        // A turn whose batch has a text delta but NO TurnDone → next_event drains
        // to None mid-turn.
        let turns = vec![vec![SessionEvent::TextDelta("partial".to_string())]];
        let mut sess = FakeSession::new(turns, false, "");
        let o = opts(tmp.path());

        let outcome = drive_director_loop(&mut sess, &o, &events, "GO".to_string()).await;
        assert!(
            matches!(outcome, DirectorLoopOutcome::Failed(_)),
            "a dead session is a Failed outcome: {outcome:?}"
        );
    }

    // ── Auto-QC units ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn auto_qc_clean_when_source_present_and_no_review_team() {
        // Real source on disk, no manifest (build/test skipped → neutral), no fork
        // (no review) → QC clean.
        let tmp = tempfile::TempDir::new().unwrap();
        seed_source(tmp.path());
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events).await;
        assert!(qc.is_clean(), "source present + nothing to fail → clean QC");
    }

    #[tokio::test]
    async fn auto_qc_blocks_when_no_source_present() {
        // No source on disk after a claimed build → the hard floor is the decisive
        // blocking finding (and QC returns early without running build/review).
        let tmp = tempfile::TempDir::new().unwrap();
        let (events, _rec) = sink();
        let mut sess = FakeSession::new(vec![], false, "");
        let o = opts(tmp.path());
        let qc = run_auto_qc(&mut sess, &o, &events).await;
        assert!(!qc.is_clean(), "no source → blocking");
        assert!(
            qc.blocking.iter().any(|b| b.contains("source-present")),
            "the hard-floor finding is present: {:?}",
            qc.blocking
        );
    }

    #[test]
    fn build_test_blocking_is_none_when_skipped_or_passed() {
        // An unavailable (skipped) check is neutral, not a false failure (fail-open).
        let skipped = VerifyResult {
            available: false,
            passed: true,
            evidence: vec![],
        };
        assert!(build_test_blocking(&skipped).is_none());
        // A passing check is not blocking.
        let ok = VerifyResult {
            available: true,
            passed: true,
            evidence: vec!["build: ok".into()],
        };
        assert!(build_test_blocking(&ok).is_none());
        // A real failure is blocking, carrying the evidence.
        let bad = VerifyResult {
            available: true,
            passed: false,
            evidence: vec!["build: FAILED (exit 1)".into()],
        };
        let line = build_test_blocking(&bad).expect("a failed step blocks");
        assert!(line.contains("FAILED") && line.contains("exit 1"));
    }

    #[test]
    fn fix_directive_lists_every_blocking_finding() {
        let qc = QcReport {
            blocking: vec![
                "verify build-test: FAILED — build: FAILED (exit 1)".into(),
                "[security] no input validation".into(),
            ],
        };
        let d = qc.fix_directive();
        assert!(d.contains("must be fixed"));
        assert!(d.contains("build: FAILED"));
        assert!(d.contains("no input validation"));
    }
}
