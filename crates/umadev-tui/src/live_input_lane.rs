//! Live input that an ended turn never delivered goes back to the user.
//!
//! A same-turn steer or a native prompt-queue entry is accepted into the
//! turn's bounded lane the moment the user presses Enter, but the turn reads
//! that lane only between base events. When the turn ends first (it finishes,
//! fails, or is cancelled), the registration is dropped with the request still
//! buffered, or with the one it was delivering unconfirmed. Dropping the
//! registration hands those requests back through the hub, and the event loop
//! puts their text back into the input box instead of losing it silently.

use crate::app::App;
use crate::{LiveInputHub, LiveInputHubState, LiveInputRegistration, LiveInputRequest};
use crate::PromptQueueRequest;

impl LiveInputRegistration {
    /// The next accepted request. It counts as in flight until [`Self::settle`].
    pub(crate) async fn recv(&mut self) -> Option<LiveInputRequest> {
        let request = self.receiver.recv().await?;
        self.in_flight = Some(request.clone());
        Some(request)
    }

    /// The in-flight request reached a terminal answer (a delivery receipt or
    /// a rejection the loop already restores), so it is no longer handed back.
    pub(crate) fn settle(&mut self) {
        self.in_flight = None;
    }

    /// Move every undelivered request into the hub. The caller holds the hub
    /// lock with the endpoint already detached, so nothing new can arrive.
    pub(crate) fn hand_back(&mut self, state: &mut LiveInputHubState) {
        state.returned.extend(self.in_flight.take());
        self.receiver.close();
        while let Ok(request) = self.receiver.try_recv() {
            state.returned.push(request);
        }
    }
}

impl LiveInputHub {
    fn take_returned(&self) -> Vec<LiveInputRequest> {
        std::mem::take(
            &mut self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .returned,
        )
    }
}

/// Put the text of every handed-back request into the input box, with a note
/// saying it was not sent. Returns whether anything was restored.
pub(crate) fn restore_returned_live_input(app: &mut App, hub: &LiveInputHub) -> bool {
    let returned = hub.take_returned();
    let restored = !returned.is_empty();
    for request in returned {
        let note = umadev_i18n::t(app.lang, "input.steer.returned").to_string();
        match request {
            LiveInputRequest::Steer { turn }
            | LiveInputRequest::PromptQueue {
                request: PromptQueueRequest::Enqueue { turn, .. },
            } => app.reject_live_input(turn, note),
            LiveInputRequest::PromptQueue {
                request: PromptQueueRequest::Mutate(mutation),
            } => app.reject_prompt_queue_mutation(mutation, note),
        }
    }
    restored
}
