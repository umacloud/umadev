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

use umadev_runtime::AskUserQuestion;

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

/// The localized prompt note: a header line + the neutral question/option block
/// + a relay hint. Pure given the process locale.
#[must_use]
pub fn note_for(q: &AskUserQuestion) -> String {
    let mut s = umadev_i18n::tlf("ask.prompt.header", &[]);
    s.push('\n');
    s.push_str(&q.prompt_block());
    s.push('\n');
    s.push_str(&umadev_i18n::tlf("ask.prompt.relay_hint", &[]));
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
}
