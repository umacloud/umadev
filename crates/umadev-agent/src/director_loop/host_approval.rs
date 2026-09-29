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
/// the request is denied — as in the chat lane. Every other approval goes
/// through the ordinary [`resolve_approval`] floor.
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
    match crate::interaction::request_approval(action, target).await {
        // Allowed once: the base asks again next time, so nothing is remembered.
        Some(true) => ResolvedApproval {
            decision: ApprovalDecision::Allow,
            headless: false,
        },
        Some(false) => {
            events.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.pause.denied",
                &[action, target],
            )));
            ResolvedApproval {
                decision: ApprovalDecision::Deny,
                headless: false,
            }
        }
        None => ResolvedApproval {
            decision: ApprovalDecision::Deny,
            headless: true,
        },
    }
}
