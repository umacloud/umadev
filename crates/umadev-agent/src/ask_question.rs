//! Bridge for the base's interactive **`AskUserQuestion`** tool.
//!
//! UmaDev drives the base **non-interactively** (claude `--print` / the
//! continuous stream-json session, codex / opencode likewise). When the base
//! calls its OWN `AskUserQuestion` — a structured multiple-choice question — it
//! cannot pop up its own picker without a TTY, so the call auto-cancels mid-turn
//! and the base proceeds as if it got no answer. UmaDev only *observes* the
//! tool-call event, so previously it rendered a bare "AskUserQuestion" stub with
//! NO options and the turn silently read as cancelled — the user never saw the
//! question or the choices.
//!
//! **What is feasible.** The base runs its own `AskUserQuestion` internally; the
//! only mid-turn control channel UmaDev has is the `can_use_tool` permission
//! prompt (allow / deny — not the structured answer). So UmaDev cannot inject a
//! mid-turn tool-result for the base's own picker. What it CAN do — and what this
//! module enables — is:
//!
//! 1. **Render** the question + its numbered options the moment the tool call is
//!    observed ([`surface`]), so the user sees exactly what's being asked.
//! 2. **Relay** the user's choice back to the base as the next turn of the SAME
//!    session ([`relay_directive`]) — the base kept the question in its own
//!    context when it asked it, so a follow-up "the user chose: <option>" turn
//!    lets it continue with the answer instead of a silent cancel.
//!
//! Fail-open throughout: a non-question tool call, or an input shape we can't
//! read, yields `None` and the caller keeps its existing tool-row rendering.

use std::sync::atomic::{AtomicBool, Ordering};

use umadev_runtime::{AskUserQuestion, ExitPlanMode};

/// Process-global "the user prefers free-text (prose) approval questions over the
/// numbered multiple-choice picker" flag. Published once at startup (and on a live
/// `/questions` toggle) by the TUI from `UserConfig::question_form`, and read here
/// when a base `AskUserQuestion` note is built — the note's three emit sites live
/// deep in the run pumps (continuous / director loops) with no config in hand, so
/// a set-once shared flag threads the preference the same way the process-log flag
/// does. Deterministic given the flag; defaults `false` (the numbered picker), so
/// existing users are unaffected until they opt in.
static PREFER_TEXT_QUESTIONS: AtomicBool = AtomicBool::new(false);

/// Publish the user's approval-question presentation preference: `true` = frame
/// questions as free-text prose, `false` (default) = the numbered picker. Called
/// by the TUI at startup and on the `/questions` toggle.
pub fn set_prefer_text_questions(on: bool) {
    PREFER_TEXT_QUESTIONS.store(on, Ordering::Relaxed);
}

/// Whether the user prefers free-text (prose) approval questions over the numbered
/// picker (see [`set_prefer_text_questions`]). Default `false`.
#[must_use]
pub fn prefers_text_questions() -> bool {
    PREFER_TEXT_QUESTIONS.load(Ordering::Relaxed)
}

/// The user-facing surface of a base `AskUserQuestion` call: the one-line
/// tool-row `detail`, and a localized multi-line `note` that shows the question
/// with its numbered options and tells the user their reply will be relayed to
/// the base (so the call no longer reads as a silent, optionless cancel).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AskQuestionSurface {
    /// One-line summary for the tool row's `(arg)` (never multi-line).
    pub detail: String,
    /// The prominent, localized multi-line prompt to emit as a `Note`.
    pub note: String,
}

/// The user-facing surface of a base `ExitPlanMode` call: the one-line tool-row
/// `detail` (the plan's first line, clipped) and a localized multi-line `note`
/// that renders the full plan markdown under a header labeling it clearly as the
/// **base's** plan mode — never conflated with UmaDev's own guarded banner.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ExitPlanSurface {
    /// One-line summary for the tool row's `(arg)` (never multi-line).
    pub detail: String,
    /// The prominent, localized multi-line note to emit (base-plan-mode header +
    /// the full plan markdown).
    pub note: String,
}

/// Build the [`AskQuestionSurface`] for a base tool call, or `None` when the call
/// is not an `AskUserQuestion` / its input can't be parsed. Localized via the
/// process locale ([`umadev_i18n::tlf`]) to match the surrounding run/chat Notes.
#[must_use]
pub fn surface(name: &str, input: &serde_json::Value) -> Option<AskQuestionSurface> {
    let q = AskUserQuestion::from_tool_input(name, input)?;
    Some(AskQuestionSurface {
        detail: q.summary(),
        note: note_for(&q),
    })
}

/// The localized prompt note: a header line + the question/option block + a hint.
/// Presentation follows the user's [`prefers_text_questions`] preference — this
/// thin wrapper reads the shared flag, then delegates to the pure
/// [`note_for_with`].
#[must_use]
pub fn note_for(q: &AskUserQuestion) -> String {
    note_for_with(q, prefers_text_questions())
}

/// [`surface`] for a **mid-run director build** (A2#6): same detail/question
/// block, but the hint is the HONEST mid-run variant — on the director path the
/// base's `AskUserQuestion` call auto-cancels and the build CONTINUES with the
/// base's own default; a typed reply is folded in as follow-up steering at the
/// next step boundary, not relayed as the live answer. The chat surface keeps
/// [`surface`] (there the pending-ask machinery really does relay the reply).
#[must_use]
pub fn surface_mid_run(name: &str, input: &serde_json::Value) -> Option<AskQuestionSurface> {
    let q = AskUserQuestion::from_tool_input(name, input)?;
    let mut note = umadev_i18n::tlf("ask.prompt.header", &[]);
    note.push('\n');
    if prefers_text_questions() {
        note.push_str(&q.prose_block());
    } else {
        note.push_str(&q.prompt_block());
    }
    note.push('\n');
    note.push_str(&umadev_i18n::tlf("ask.prompt.midrun_hint", &[]));
    Some(AskQuestionSurface {
        detail: q.summary(),
        note,
    })
}

/// The pure body of [`note_for`], parameterized on `prefer_text` so the framing is
/// testable without the process-global flag.
///
/// - `false` (the default picker): header + the NUMBERED option block +
///   "reply with the option number" relay hint.
/// - `true` (text-question mode): header + the BULLETED option block (no numbered
///   pick framing) + an "answer in your own words" hint. The reply still flows
///   through the same relay path — only the presentation changes.
#[must_use]
pub fn note_for_with(q: &AskUserQuestion, prefer_text: bool) -> String {
    let mut s = umadev_i18n::tlf("ask.prompt.header", &[]);
    s.push('\n');
    if prefer_text {
        s.push_str(&q.prose_block());
        s.push('\n');
        s.push_str(&umadev_i18n::tlf("ask.prompt.text_hint", &[]));
    } else {
        s.push_str(&q.prompt_block());
        s.push('\n');
        s.push_str(&umadev_i18n::tlf("ask.prompt.relay_hint", &[]));
    }
    s
}

/// Build the [`ExitPlanSurface`] for a base tool call, or `None` when the call is
/// not an `ExitPlanMode` / carries no readable `plan` text. Mirrors [`surface`]:
/// fail-open, and the caller keeps its existing tool-row rendering on `None`.
#[must_use]
pub fn exit_plan_surface(name: &str, input: &serde_json::Value) -> Option<ExitPlanSurface> {
    let p = ExitPlanMode::from_tool_input(name, input)?;
    Some(ExitPlanSurface {
        detail: p.summary(),
        note: exit_plan_note(&p),
    })
}

/// The localized note for a base `ExitPlanMode`: a header that labels it as the
/// **base CLI's** plan mode (distinct from UmaDev's guarded banner) followed by
/// the full plan markdown, so the user SEES the plan being approved. Pure given
/// the process locale.
#[must_use]
pub fn exit_plan_note(p: &ExitPlanMode) -> String {
    let mut s = umadev_i18n::tlf("plan_mode.base_exit", &[]);
    s.push('\n');
    s.push_str(&p.plan);
    s
}

/// Build the next-turn directive that relays the user's `reply` to a base
/// `AskUserQuestion` back into the SAME session. The reply is resolved against
/// the asked options ([`AskUserQuestion::resolve_reply`] — a bare option number
/// or exact label maps to the canonical label; free-text passes through), then
/// framed as an explicit answer so the base continues with the choice instead of
/// the cancelled call.
#[must_use]
pub fn relay_directive(q: &AskUserQuestion, reply: &str) -> String {
    let resolved = q.resolve_reply(reply);
    let asked = q
        .questions
        .first()
        .map(|first| {
            if first.question.is_empty() {
                first.header.clone()
            } else {
                first.question.clone()
            }
        })
        .unwrap_or_default();
    if asked.is_empty() {
        format!("The user answered your AskUserQuestion: {resolved}. Continue with that choice.")
    } else {
        format!(
            "The user answered the question you asked via AskUserQuestion.\n\
             Question: {asked}\n\
             The user chose: {resolved}\n\
             Continue with that choice (do not ask it again)."
        )
    }
}

/// Resolve the directive to send for a user `reply` given the OPTIONAL pending
/// question the base last asked: `Some(q)` → the resolved + framed relay directive
/// ([`relay_directive`], so a bare `1` becomes the chosen label framed as the
/// user's explicit answer); `None` → the raw `reply` passes through unchanged.
///
/// The single seam the chat send-path calls so a numbered answer to a base
/// `AskUserQuestion` is relayed as the resolved choice instead of the ambiguous
/// bare index, while an ordinary turn (no question pending) is untouched.
/// **Fail-open:** no pending question → passthrough; an unresolvable reply still
/// passes through inside [`relay_directive`] (it never drops the user's words).
#[must_use]
pub fn relay_or_passthrough(pending: Option<&AskUserQuestion>, reply: &str) -> String {
    match pending {
        Some(q) => relay_directive(q, reply),
        None => reply.to_string(),
    }
}

/// INTERACTIVE-ONLY decision (Fix ⑤): when the base calls its OWN `AskUserQuestion`
/// (or `ExitPlanMode`), should the turn STOP draining, PARK the live session, and
/// WAIT for the user's reply — rather than the HEADLESS behaviour of merely observing
/// the call, stashing the question, and letting the turn auto-continue?
///
/// Returns `true` only when a live user is present on an interactive surface
/// (`interactive && has_user`). A HEADLESS / `/run` / autonomous / non-TTY turn ALWAYS
/// returns `false` and MUST keep today's observe-stash-and-continue behaviour — a run
/// with no user to answer must never block. Pure + deterministic so the "headless
/// never blocks" contract is a structural property of the caller, not a runtime guess.
#[must_use]
pub fn should_wait_for_question(interactive: bool, has_user: bool) -> bool {
    interactive && has_user
}

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_runtime::{AskOption, AskQuestion};

    fn sample() -> AskUserQuestion {
        AskUserQuestion {
            questions: vec![AskQuestion {
                header: "Auth".into(),
                question: "Which auth method should the app use?".into(),
                multi_select: false,
                options: vec![
                    AskOption {
                        label: "Email + password".into(),
                        description: "Classic credentials".into(),
                    },
                    AskOption {
                        label: "OAuth (Google)".into(),
                        description: String::new(),
                    },
                ],
            }],
        }
    }

    #[test]
    fn surface_renders_question_and_options_not_a_bare_stub() {
        let input = serde_json::json!({
            "questions": [{
                "header": "Auth",
                "question": "Which auth method should the app use?",
                "options": [
                    {"label": "Email + password", "description": "Classic credentials"},
                    {"label": "OAuth (Google)"}
                ]
            }]
        });
        let s = surface("AskUserQuestion", &input).expect("AskUserQuestion has a surface");
        // The one-line tool-row detail is non-empty (was a bare stub before).
        assert!(!s.detail.is_empty());
        assert!(!s.detail.contains('\n'));
        // The prominent note carries the question AND every numbered option.
        assert!(s.note.contains("Which auth method"), "note: {}", s.note);
        assert!(s.note.contains("1. Email + password"), "note: {}", s.note);
        assert!(s.note.contains("2. OAuth (Google)"), "note: {}", s.note);
    }

    #[test]
    fn surface_fails_open_for_non_question_tools() {
        let input = serde_json::json!({"file_path": "src/app.rs"});
        assert!(surface("Write", &input).is_none());
    }

    #[test]
    fn surface_mid_run_uses_the_honest_midrun_hint_not_the_relay_framing() {
        // A2#6: on the director path the base's question auto-cancels and the
        // build CONTINUES with a default — the mid-run surface must carry the
        // honest hint (answer folds in as follow-up steering), never the chat
        // relay's "the base is waiting on your answer" framing.
        let input = serde_json::json!({
            "questions": [{
                "header": "Auth",
                "question": "Which auth method should the app use?",
                "options": [
                    {"label": "Email + password"},
                    {"label": "OAuth (Google)"}
                ]
            }]
        });
        let mid = surface_mid_run("AskUserQuestion", &input).expect("has a surface");
        // Same question/options block as the chat surface…
        assert!(mid.note.contains("Which auth method"), "note: {}", mid.note);
        // …but the MID-RUN hint, not the relay hint.
        assert!(
            mid.note
                .contains(&umadev_i18n::tlf("ask.prompt.midrun_hint", &[])),
            "carries the honest mid-run hint: {}",
            mid.note
        );
        assert!(
            !mid.note
                .contains(&umadev_i18n::tlf("ask.prompt.relay_hint", &[])),
            "never claims the base is waiting on a relayed answer: {}",
            mid.note
        );
        // Fail-open parity with `surface`.
        assert!(surface_mid_run("Write", &serde_json::json!({})).is_none());
    }

    #[test]
    fn note_for_with_text_mode_drops_numbers_and_invites_free_text() {
        let q = sample();
        // Picker (default): numbered options + the numeric relay hint.
        let picker = note_for_with(&q, false);
        assert!(picker.contains("1. Email + password"), "picker: {picker}");
        // Text mode: bulleted options (no numeric pick framing) + a distinct hint.
        let text = note_for_with(&q, true);
        assert!(text.contains("- Email + password"), "text: {text}");
        assert!(
            !text.contains("1. Email + password"),
            "text mode must drop the numbered picker framing: {text}"
        );
        assert_ne!(picker, text, "the two framings differ");
    }

    #[test]
    fn exit_plan_surface_renders_the_plan_and_labels_it_base_plan_mode() {
        let input = serde_json::json!({
            "plan": "## Plan\n- Scaffold the API\n- Add auth\n- Wire the UI"
        });
        let s = exit_plan_surface("ExitPlanMode", &input).expect("ExitPlanMode has a surface");
        // The tool-row detail is a real one-line summary, not a bare name.
        assert!(!s.detail.is_empty());
        assert!(!s.detail.contains('\n'));
        // The note carries the ACTUAL plan text (not a bare "ExitPlanMode" stub).
        assert!(s.note.contains("Scaffold the API"), "note: {}", s.note);
        assert!(s.note.contains("Add auth"), "note: {}", s.note);
        // It is labeled as the BASE's plan mode (the dedicated i18n key), so it is
        // never conflated with UmaDev's own guarded banner.
        let label = umadev_i18n::tl("plan_mode.base_exit");
        assert!(
            !label.is_empty(),
            "the base-plan-mode label must be catalogued"
        );
        assert!(
            s.note.contains(label),
            "note must carry the base-plan-mode label: {}",
            s.note
        );
        // Fail-open: a non-plan / empty-plan call has no dedicated surface.
        assert!(exit_plan_surface("Write", &serde_json::json!({"file_path": "a"})).is_none());
        assert!(exit_plan_surface("ExitPlanMode", &serde_json::json!({"plan": "  "})).is_none());
    }

    #[test]
    fn relay_directive_resolves_choice_and_frames_an_answer() {
        let q = sample();
        // A bare option number is resolved to the chosen label and framed as an
        // explicit answer the base continues with — NOT a silent cancel.
        let d = relay_directive(&q, "1");
        assert!(d.contains("Email + password"), "directive: {d}");
        assert!(d.contains("Which auth method"), "carries the question: {d}");
        assert!(
            d.to_lowercase().contains("chose") || d.to_lowercase().contains("answered"),
            "framed as an answer: {d}"
        );
        // Free-text passes through.
        let d2 = relay_directive(&q, "use passkeys");
        assert!(d2.contains("use passkeys"), "directive: {d2}");
    }

    #[test]
    fn relay_or_passthrough_relays_when_pending_else_passes_raw() {
        let q = sample();
        // A pending question + a numeric reply sends the RESOLVED + framed
        // directive — NOT the bare "1" (the exact misinterpret the relay prevents).
        let relayed = relay_or_passthrough(Some(&q), "1");
        assert_ne!(relayed.trim(), "1", "must not send the bare index");
        assert!(relayed.contains("Email + password"), "resolved: {relayed}");
        assert!(
            relayed.to_lowercase().contains("chose") || relayed.to_lowercase().contains("answered"),
            "framed as the user's answer: {relayed}"
        );
        // No pending question → the raw reply passes through verbatim.
        assert_eq!(relay_or_passthrough(None, "just chatting"), "just chatting");
        // Fail-open: an unresolvable reply with a pending question still carries the
        // user's words (free-text is honored, never dropped).
        let free = relay_or_passthrough(Some(&q), "use whatever is simplest");
        assert!(free.contains("use whatever is simplest"), "free: {free}");
    }

    #[test]
    fn should_wait_for_question_only_with_a_live_interactive_user() {
        // The pause is INTERACTIVE-ONLY: only a live user on an interactive surface
        // parks + waits. Every other combination (headless / non-TTY / no user) keeps
        // today's observe-stash-and-continue behaviour so a userless run never blocks.
        assert!(should_wait_for_question(true, true));
        assert!(
            !should_wait_for_question(false, true),
            "a non-interactive (headless / /run) turn must never wait"
        );
        assert!(
            !should_wait_for_question(true, false),
            "no user present ⇒ never wait"
        );
        assert!(!should_wait_for_question(false, false));
    }
}
