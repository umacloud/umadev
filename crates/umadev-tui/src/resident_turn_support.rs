use std::path::Path;
use std::sync::Arc;

use umadev_agent::{ChannelSink, EngineEvent, EventSink, RoutePlan};
use umadev_runtime::{
    BasePermissionProfile, DeliveryReceiptStage, DeliveryReport, InputDelivery, SessionError,
    TurnInput, TurnInputBlock, TurnInputBlockKind,
};

use crate::interaction_bridge::PendingAskHolder;

use super::{RouteDecision, SubmittedTurn, TYPED_USER_INPUT_SLOT};

/// Select the exact payload for one resident turn.
///
/// Base-native commands are out-of-band control frames: they retain their exact
/// bytes and leave a pending model question untouched. Ordinary chat consumes
/// that question and rewrites a sole text block only when it relays the answer.
pub(super) async fn select_resident_turn_payload(
    native_command: bool,
    original_text: String,
    input: TurnInput,
    pending_ask: &PendingAskHolder,
) -> (String, TurnInput) {
    if native_command {
        return (original_text, input);
    }
    let text = {
        let pending = pending_ask.lock().await.take();
        umadev_agent::ask_question_relay_or_passthrough(pending.as_ref(), &original_text)
    };
    let input = if input.sole_text() == Some(original_text.as_str()) && text != original_text {
        TurnInput::text(text.clone())
    } else {
        input
    };
    (text, input)
}

pub(super) fn route_clarification_reply(question: &umadev_agent::ClarifyQuestion) -> String {
    let mut out = question.question.trim().to_string();
    for (index, option) in question.options.iter().enumerate() {
        let option = option.trim();
        if !option.is_empty() {
            out.push_str(&format!("\n{}. {option}", index + 1));
        }
    }
    out
}

pub(super) fn route_fallback_note(
    route: &RoutePlan,
    reason: umadev_agent::RouteFallbackReason,
) -> String {
    format!(
        "[route] 意图模型未完成判断（{}），已采用确定性安全路径：{} / {}。",
        reason.as_str(),
        route.class.as_str(),
        route.depth.as_str()
    )
}

pub(super) fn capture_resident_tool_pitfall(
    project_root: &Path,
    slug: &str,
    requirement: &str,
    summary: &str,
    sink: &Arc<ChannelSink>,
) {
    if summary.trim().is_empty() {
        return;
    }
    let outcome = umadev_agent::lessons::capture_dev_errors_detailed(
        project_root,
        &[summary.to_string()],
        slug,
        requirement,
    );
    for note in outcome.progress_notes() {
        sink.emit(EngineEvent::Note(note));
    }
}

pub(super) fn routed_turn_executes_read_only(
    native_command: bool,
    route: &RoutePlan,
    _source: Option<umadev_agent::RouteSource>,
    explicit_read_only: bool,
    permissions: BasePermissionProfile,
) -> bool {
    if native_command {
        return false;
    }
    if explicit_read_only || permissions == BasePermissionProfile::Plan {
        return true;
    }
    !route.class.mutates_workspace()
}

pub(super) fn directive_turn_input(
    template: &str,
    user: &TurnInput,
) -> Result<TurnInput, SessionError> {
    let Some((prefix, suffix)) = template.split_once(TYPED_USER_INPUT_SLOT) else {
        return Err(SessionError::InputInvalid {
            index: 0,
            kind: TurnInputBlockKind::Text,
            reason: "internal typed-input slot is missing".to_string(),
        });
    };
    if suffix.contains(TYPED_USER_INPUT_SLOT) || user.blocks.is_empty() {
        return Err(SessionError::InputInvalid {
            index: 0,
            kind: TurnInputBlockKind::Text,
            reason: "internal typed-input slot is ambiguous".to_string(),
        });
    }
    let mut blocks = user.blocks.clone();
    if !prefix.is_empty() {
        if let Some(TurnInputBlock::Text { text }) = blocks.first_mut() {
            text.insert_str(0, prefix);
        } else {
            blocks.insert(
                0,
                TurnInputBlock::Text {
                    text: prefix.to_string(),
                },
            );
        }
    }
    if !suffix.is_empty() {
        if let Some(TurnInputBlock::Text { text }) = blocks.last_mut() {
            text.push_str(suffix);
        } else {
            blocks.push(TurnInputBlock::Text {
                text: suffix.to_string(),
            });
        }
    }
    Ok(TurnInput::new(blocks))
}

fn input_kind_label(kind: TurnInputBlockKind) -> &'static str {
    match kind {
        TurnInputBlockKind::Text => umadev_i18n::tl("input.kind.text"),
        TurnInputBlockKind::Image => umadev_i18n::tl("input.kind.image"),
        TurnInputBlockKind::File => umadev_i18n::tl("input.kind.file"),
    }
}

fn delivery_label(delivery: InputDelivery) -> &'static str {
    match delivery {
        InputDelivery::Native => umadev_i18n::tl("input.delivery.native"),
        InputDelivery::MaterializedText => umadev_i18n::tl("input.delivery.materialized_text"),
        InputDelivery::Unsupported => umadev_i18n::tl("input.delivery.unsupported"),
    }
}

fn compact_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        let tenths = bytes.saturating_mul(10) / (1024 * 1024);
        format!("{}.{:01} MiB", tenths / 10, tenths % 10)
    } else if bytes >= 1024 {
        let tenths = bytes.saturating_mul(10) / 1024;
        format!("{}.{:01} KiB", tenths / 10, tenths % 10)
    } else {
        format!("{bytes} B")
    }
}

pub(super) fn delivery_report_status(report: &DeliveryReport) -> String {
    let blocks = report
        .blocks
        .iter()
        .map(|block| {
            let mime = block
                .media_type
                .as_deref()
                .filter(|_| block.kind != TurnInputBlockKind::Text)
                .map_or_else(String::new, |mime| format!(" · {mime}"));
            format!(
                "#{} {}={} · {}{}",
                block.index + 1,
                input_kind_label(block.kind),
                delivery_label(block.delivery),
                compact_bytes(block.source_bytes),
                mime
            )
        })
        .collect::<Vec<_>>()
        .join("  |  ");
    let key = match report.receipt {
        DeliveryReceiptStage::TransportWritten => "input.delivery.receipt",
        DeliveryReceiptStage::ProtocolAcknowledged => "input.delivery.protocol_acknowledged",
    };
    umadev_i18n::tlf(key, &[&blocks])
}

pub(super) fn input_failure_note(backend: &str, error: &SessionError) -> String {
    match error {
        SessionError::InputUnsupported { index, kind, .. } => umadev_i18n::tlf(
            "input.delivery.rejected",
            &[
                &(index + 1).to_string(),
                input_kind_label(*kind),
                umadev_i18n::tl("input.delivery.unsupported"),
                umadev_i18n::tl("input.delivery.unsupported_help"),
            ],
        ),
        SessionError::InputInvalid { index, kind, .. } => umadev_i18n::tlf(
            "input.delivery.rejected",
            &[
                &(index + 1).to_string(),
                input_kind_label(*kind),
                umadev_i18n::tl("input.delivery.invalid"),
                umadev_i18n::tl("input.delivery.invalid_help"),
            ],
        ),
        _ => umadev_i18n::tlf("chat.turn_failed", &[backend, &error.to_string()]),
    }
}

pub(super) fn input_failure_decision(
    text: &str,
    input: &TurnInput,
    backend: &str,
    error: &SessionError,
) -> RouteDecision {
    let note = input_failure_note(backend, error);
    let turn = SubmittedTurn {
        text: text.to_string(),
        input: input.clone(),
    };
    if turn.has_attachments() {
        RouteDecision::InputRejected { turn, note }
    } else {
        RouteDecision::Failed(note)
    }
}

#[cfg(test)]
mod route_fallback_tests {
    use super::*;

    #[test]
    fn fallback_note_names_the_real_cause_and_final_route() {
        let route = umadev_agent::deterministic_route("where is the repo");
        let note = route_fallback_note(&route, umadev_agent::RouteFallbackReason::TurnTimedOut);
        assert!(note.contains("turn-timeout"));
        assert!(note.contains(route.class.as_str()));
        assert!(note.contains(route.depth.as_str()));
    }
}
