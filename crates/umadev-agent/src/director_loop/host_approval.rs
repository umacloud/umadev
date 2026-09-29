//! Typed base approvals on the `/run` and continuous lanes.

use std::sync::Arc;

use umadev_runtime::{ApprovalDecision, HostRequest};

use super::{resolve_approval, ResolvedApproval};
use crate::events::{EngineEvent, EventSink};
use crate::runner::RunOptions;

/// Resolve a typed [`HostRequest::Approval`].
///
/// The host flags `upstreamPermissionBoundary` on a request the base sent even
/// though UmaDev asked it for Auto: the base's own policy (a user or managed
/// ask rule) still demands a confirmation. Neither the project trust ledger nor
/// the Auto release can answer that, so the live user decides, and without one
/// the request is denied, as in the chat lane. The host is told that only the
/// user's answer settles it, so switching to Auto cannot release it either.
/// Nothing is remembered either way, since the base asks again next time.
/// Every other approval goes through the ordinary [`resolve_approval`].
pub(super) async fn resolve(
    options: &RunOptions,
    events: &Arc<dyn EventSink>,
    request: &HostRequest,
) -> ResolvedApproval {
    let HostRequest::Approval {
        action,
        target,
        metadata,
        ..
    } = request
    else {
        return ResolvedApproval {
            decision: ApprovalDecision::Deny,
            headless: true,
        };
    };
    if metadata
        .get("upstreamPermissionBoundary")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return resolve_approval(options, events, action, target).await;
    }
    let (decision, headless, note) =
        match crate::interaction::request_user_answer(action, target).await {
            Some(true) => (ApprovalDecision::Allow, false, "trust.pause.allowed"),
            Some(false) => (ApprovalDecision::Deny, false, "trust.pause.denied"),
            None => (
                ApprovalDecision::Deny,
                true,
                "continuous.dangerous_action_denied",
            ),
        };
    events.emit(EngineEvent::Note(umadev_i18n::tlf(note, &[action, target])));
    ResolvedApproval { decision, headless }
}
