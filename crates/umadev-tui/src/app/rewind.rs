//! Idle double-Esc rewind: re-load the last user message for editing.

use super::{App, ChatRole};

impl App {
    /// Index of the most recent user (`You`) turn in the transcript, or `None`
    /// when the user has not spoken yet. Drives the idle double-Esc rewind.
    #[must_use]
    pub(super) fn last_user_msg_index(&self) -> Option<usize> {
        self.history.iter().rposition(|m| m.role == ChatRole::You)
    }

    /// Rewind the CHAT transcript to the last user turn: re-load that message's
    /// text into the input box for editing and drop it plus every turn after it,
    /// so a resend re-asks from that point. Chat-only — it does NOT roll back
    /// files or run state (that is the engine's `checkpoint`, out of scope).
    /// Fail-open: a no-op when there is no prior user turn.
    pub(super) fn rewind_to_last_user_message(&mut self) {
        let Some(idx) = self.last_user_msg_index() else {
            return;
        };
        // `idx` is valid (just found by `rposition`); `body()` is the plain text
        // of a `You` row. Drop the user turn + everything after it, then re-load
        // its text for editing.
        let text = self.history[idx].body().into_owned();
        self.history.truncate(idx);
        // Keep the base-facing memory + durable transcript in lockstep with the
        // visible rewind, so a resend does not re-ask WITH the dropped turn and a
        // relaunch `/resume` does not restore it. Each vector is truncated at its
        // OWN last `user` entry (compaction can desync their lengths) — but only
        // when that entry IS the rewound row. Slash commands and gate / approval
        // replies are shown as `You` but never recorded; cutting memory at its
        // last recorded turn then silently dropped a different exchange that
        // was still on screen.
        let rewound = |turns: &[umadev_runtime::Message]| {
            turns
                .iter()
                .rposition(|m| m.role == "user")
                .filter(|&at| turns[at].content.trim() == text.trim())
        };
        let transcript_cut = rewound(&self.full_transcript);
        if let Some(at) = rewound(&self.conversation) {
            self.conversation.truncate(at);
        }
        if let Some(at) = transcript_cut {
            self.full_transcript.truncate(at);
        }
        // Mirror the rewind to disk. When the rewound turn was the FIRST recorded
        // one the transcript is now EMPTY, and `persist_chat` early-returns on an
        // empty transcript (to avoid empty-file litter) — which would leave the
        // OLD, un-rewound chat on disk for a relaunch `/resume` to restore. Delete
        // the persisted chat only in that case; a chat whose recorded turns are
        // still on screen is rewritten, never deleted.
        if transcript_cut.is_some() && self.full_transcript.is_empty() {
            self.discard_persisted_chat();
        } else {
            self.persist_chat();
        }
        self.input = text;
        self.input_cursor = self.input_len();
        // Leave history recall + the quit/rewind arms in a clean state, and
        // re-pin the transcript to the bottom so the freshly truncated tail shows.
        self.input_history_idx = None;
        self.pending_quit_confirm = false;
        self.pending_rewind = false;
        self.transcript_scroll_to_bottom();
    }
}
