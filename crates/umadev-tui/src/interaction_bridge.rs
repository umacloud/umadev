//! Same-RPC interaction bridge for resident base sessions.
//!
//! This module owns typed host input, guarded approvals, live trust-mode
//! propagation, and protocol-shaped responses while the originating base
//! request remains open.

use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use umadev_agent::{ChannelSink, EngineEvent, EventSink};

use crate::app::{
    host_input::{HostInputDescriptor, HostInputKeyOutcome},
    App,
};

/// Holds the base's most-recent **`AskUserQuestion`** across turns so the user's
/// NEXT chat line is relayed back as a resolved, framed answer rather than the raw
/// (and easily-misread) bare option number. Set when the chat drain surfaces a
/// base question; taken + cleared at the start of the next turn, which relays the
/// user's reply through [`umadev_agent::ask_question_relay_or_passthrough`]. Shared
/// `Arc` between the event loop and the spawned chat-turn tasks (a
/// `tokio::sync::Mutex` so a task can take it across `.await`). Fail-open: an empty
/// holder means the line is sent verbatim.
pub(super) type PendingAskHolder = Arc<tokio::sync::Mutex<Option<umadev_runtime::AskUserQuestion>>>;

/// The user's verdict on a paused Guarded consequential-action approval (Fix ③).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum ApprovalReply {
    /// Let the action run (and remember its class so it is not re-asked).
    Allow,
    /// Skip the action. The default on ANY fail-open path (Esc / cancel / the wait
    /// budget elapsed / a dropped channel) so the base is never left hanging.
    Deny,
}

/// A resident chat turn PAUSED on a Guarded consequential-action approval (Fix ③),
/// waiting for the live user's `y` / `n` / Esc keypress. The drain task registers one
/// (its `reply_tx`), the event loop routes the user's decision into it, and the drain
/// then `respond`s to the base's `req_id`.
///
/// **Interactive-only, by construction:** a [`PendingApproval`] is registered ONLY on
/// the interactive resident-chat drain when [`umadev_agent::guarded_should_pause_item`]
/// says so — a HEADLESS / `/run` / non-TTY turn never creates one and never blocks.
pub(super) struct PendingApproval {
    /// One-shot channel the event loop sends the user's [`ApprovalReply`] through.
    /// Dropping it (cancel / quit / a cleared holder) makes the drain's `await`
    /// fail-open to [`ApprovalReply::Deny`] — the "no hang" guarantee.
    pub(super) reply_tx: tokio::sync::oneshot::Sender<ApprovalReply>,
    /// What the base wants to do (e.g. `Bash`) — carried so the event loop can
    /// mirror the pause into the app model and the renderer can pin a VISIBLE
    /// sticky approval bar above the input box (A2#5: the pause used to surface
    /// only as one scrolling Note with no persistent approval entry point).
    pub(super) action: String,
    /// The action's target (e.g. `npm install`), same purpose as `action`.
    pub(super) target: String,
    /// Whether switching the local trust tier to Auto may release this pause.
    ///
    /// `false` is reserved for a permission request that the selected base
    /// still emitted while UmaDev had requested Auto. Such a request is an
    /// upstream policy/sandbox boundary, not an ordinary Guarded prompt, and
    /// only an explicit user verdict may resolve it.
    pub(super) auto_releasable: bool,
    /// The base protocol `req_id` this approval answers, when it came from a
    /// `HostRequest::Approval` (empty for a Guarded local-pause that has no base
    /// RPC). Retained so a [`umadev_runtime::SessionEvent::HostRequestSettled`]
    /// can retract exactly this approval when the base withdraws its request.
    pub(super) req_id: String,
}

/// Shared slot for the single in-flight [`PendingApproval`]. A plain `std::sync::Mutex`
/// (not tokio) because it is locked only for the nanoseconds it takes to store / take /
/// send — never held across an `.await` — so the sync event-loop key handler can poke it
/// without an async lock. `None` = no approval pending (the common case).
pub(super) type ApprovalHolder = Arc<std::sync::Mutex<Option<PendingApproval>>>;

/// One typed question/elicitation whose originating base RPC is still open.
/// The event loop validates the composer's text and returns the exact typed
/// response through this one-shot; dropping the slot safely rejects the request
/// instead of allowing an unknown prompt to inherit authority.
pub(super) struct PendingHostInput {
    pub(super) token: u64,
    /// The base protocol `req_id` this picker is answering. Retained so a
    /// [`umadev_runtime::SessionEvent::HostRequestSettled`] (the base withdrew
    /// the request) can be correlated to THIS picker and retract it.
    pub(super) req_id: String,
    pub(super) reply_tx: tokio::sync::oneshot::Sender<umadev_runtime::HostResponse>,
    pub(super) request: umadev_runtime::HostRequest,
}

/// Shared single-request bridge between an in-flight base drain and the TUI
/// input loop. A synchronous mutex is sufficient because every lock is held only
/// while cloning metadata or taking the sender, never across an await.
pub(super) type HostInputHolder = Arc<std::sync::Mutex<Option<PendingHostInput>>>;

pub(super) static NEXT_HOST_INPUT_TOKEN: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

/// Whether the pending request contains a secret question. If any of a grouped
/// set is secret the whole composer is masked, preventing adjacent answers from
/// exposing where the sensitive value begins or ends.
pub(super) fn host_request_is_secret(request: &umadev_runtime::HostRequest) -> bool {
    matches!(
        request,
        umadev_runtime::HostRequest::UserInput { questions, .. }
            if questions.iter().any(|question| {
                matches!(question.kind, umadev_runtime::HostQuestionKind::Secret)
            })
    )
}

/// Compact one-line form pinned above the input. Full prompts and options are
/// emitted into the transcript by [`await_host_input`].
pub(super) fn host_request_summary(request: &umadev_runtime::HostRequest) -> String {
    match request {
        umadev_runtime::HostRequest::UserInput { questions, .. } => questions.first().map_or_else(
            || "base requested user input".to_string(),
            |question| {
                question.header.as_ref().map_or_else(
                    || question.prompt.clone(),
                    |header| format!("{header}: {}", question.prompt),
                )
            },
        ),
        umadev_runtime::HostRequest::McpElicitation {
            server_name,
            message,
            ..
        } => server_name
            .as_ref()
            .map_or_else(|| message.clone(), |server| format!("{server}: {message}")),
        umadev_runtime::HostRequest::FolderTrust { workspace, .. } => {
            format!("Grok Build folder trust: {}", workspace.display())
        }
        _ => "base requested a response".to_string(),
    }
}

/// Full visible prompt for a typed request. It includes stable numbered option
/// labels while retaining protocol values internally; no metadata/payload is
/// printed because vendor fields may contain sensitive material.
pub(super) fn host_request_note(request: &umadev_runtime::HostRequest) -> String {
    match request {
        umadev_runtime::HostRequest::UserInput {
            questions,
            metadata,
        } => {
            // A contract-driven ask is answered through the SPECIALIZED PICKER,
            // which owns every bare key — typed free-form text is never read.
            // Teaching the free-form protocol for those asks sent users typing
            // "A" + Enter into a picker that cannot see it (the reported
            // invalid-response loop); the tail instruction must match the real
            // input surface.
            let contract_picker = metadata
                .get("responseContract")
                .and_then(serde_json::Value::as_str)
                .is_some();
            let mut out = String::from("[input] ");
            if questions.len() > 1 {
                out.push_str("The base is waiting for structured answers:\n");
            }
            for (question_index, question) in questions.iter().enumerate() {
                if question_index > 0 {
                    out.push('\n');
                }
                if questions.len() > 1 {
                    out.push_str(&format!("{}. ", question_index + 1));
                }
                if let Some(header) = &question.header {
                    out.push_str(header);
                    out.push_str(": ");
                }
                out.push_str(&question.prompt);
                for (option_index, option) in question.options.iter().enumerate() {
                    out.push_str(&format!("\n   {}. {}", option_index + 1, option.label));
                    if let Some(description) = &option.description {
                        out.push_str(" — ");
                        out.push_str(description);
                    }
                }
            }
            if contract_picker {
                out.push_str(
                    "\nAnswer in the picker above the input box (↑↓ move · Space select · Enter confirm · Tab note) — typed free-form text is not read there.",
                );
            } else if questions.len() > 1 {
                out.push_str(
                    "\nReply with one line per question, or a JSON object keyed by question id.",
                );
            }
            out
        }
        umadev_runtime::HostRequest::McpElicitation {
            server_name,
            message,
            requested_schema,
            ..
        } => {
            let server = server_name.as_deref().unwrap_or("MCP");
            let expected = requested_schema
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("JSON");
            format!("[input] {server}: {message}\nExpected response type: {expected}")
        }
        umadev_runtime::HostRequest::FolderTrust {
            cwd,
            workspace,
            config_kinds,
        } => format!(
            "[folder trust] Grok Build is waiting before loading project configuration.\nWorkspace: {}\nSession cwd: {}\nConfiguration: {}\nTrust requires a separate explicit confirmation; Auto mode never answers this request.",
            workspace.display(),
            cwd.display(),
            config_kinds.join(", ")
        ),
        _ => format!("[input] {}", host_request_summary(request)),
    }
}

pub(super) fn pending_host_input_item(holder: &HostInputHolder) -> Option<HostInputDescriptor> {
    holder.lock().ok().and_then(|guard| {
        guard.as_ref().map(|pending| HostInputDescriptor {
            token: pending.token,
            request: pending.request.clone(),
        })
    })
}

/// Drop a pending typed request. The waiter maps the closed one-shot to the
/// request's safe rejection; this is used by cancel/quit/backend-switch cleanup.
pub(super) fn clear_pending_host_input(holder: &HostInputHolder) {
    if let Ok(mut guard) = holder.lock() {
        guard.take();
    }
}

pub(super) fn clear_pending_host_input_if(holder: &HostInputHolder, token: u64) {
    if let Ok(mut guard) = holder.lock() {
        if guard.as_ref().is_some_and(|pending| pending.token == token) {
            guard.take();
        }
    }
}

/// Retract a pending picker IFF it is answering `req_id` — the base WITHDREW that
/// request (cancel / sub-agent finished / timeout). Settles the parked RPC with a
/// protocol-shaped cancel so its waiter (if any) wakes cleanly, and clears the
/// slot so the per-frame UI sync drops the picker. Req-id-scoped: a newer request
/// that already superseded this one keeps its live picker. Returns whether a
/// matching picker was retracted.
pub(super) fn retract_pending_host_input_for(holder: &HostInputHolder, req_id: &str) -> bool {
    let Ok(mut guard) = holder.lock() else {
        return false;
    };
    if guard
        .as_ref()
        .is_some_and(|pending| pending.req_id == req_id)
    {
        if let Some(pending) = guard.take() {
            // A withdrawal settles as a uniform protocol Cancelled (universally
            // handled by every driver's write-back), never a fabricated answer —
            // the base isn't listening anyway, but this keeps the waiter's return
            // path honest.
            let _ = pending
                .reply_tx
                .send(umadev_runtime::HostResponse::Cancelled {
                    reason: Some("the base withdrew this request".to_string()),
                });
        }
        return true;
    }
    false
}

/// The approval twin of [`retract_pending_host_input_for`]: retract a pending
/// approval that was answering `req_id`. An empty `req_id` (a Guarded local pause
/// / host-git / `/run` approval with no base RPC) never matches a real withdrawn
/// id. Returns whether a matching approval was retracted.
pub(super) fn retract_pending_approval_for(holder: &ApprovalHolder, req_id: &str) -> bool {
    if req_id.is_empty() {
        return false;
    }
    let Ok(mut guard) = holder.lock() else {
        return false;
    };
    if guard
        .as_ref()
        .is_some_and(|pending| pending.req_id == req_id)
    {
        if let Some(pending) = guard.take() {
            let _ = pending.reply_tx.send(ApprovalReply::Deny);
        }
        return true;
    }
    false
}

pub(super) fn is_host_cancel_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "cancel" | "/cancel" | "取消" | "取消回答" | "取消答覆"
    )
}

pub(super) fn resolve_host_choice(
    token: &str,
    options: &[umadev_runtime::HostQuestionOption],
) -> std::result::Result<String, String> {
    let token = token.trim();
    if let Ok(index) = token.parse::<usize>() {
        if let Some(option) = index.checked_sub(1).and_then(|index| options.get(index)) {
            return Ok(option.value.clone());
        }
    }
    options
        .iter()
        .find(|option| {
            option.value.eq_ignore_ascii_case(token) || option.label.eq_ignore_ascii_case(token)
        })
        .map(|option| option.value.clone())
        .ok_or_else(|| format!("unknown choice `{token}`"))
}

pub(super) fn parse_host_question_answer(
    question: &umadev_runtime::HostQuestion,
    raw: &str,
) -> std::result::Result<umadev_runtime::HostAnswer, String> {
    let raw = raw.trim();
    if question.required && raw.is_empty() {
        return Err(format!("`{}` requires an answer", question.id));
    }
    let values = match &question.kind {
        umadev_runtime::HostQuestionKind::Text
        | umadev_runtime::HostQuestionKind::Secret
        | umadev_runtime::HostQuestionKind::Other(_) => {
            if raw.is_empty() {
                Vec::new()
            } else {
                vec![raw.to_string()]
            }
        }
        umadev_runtime::HostQuestionKind::SingleChoice => {
            if raw.is_empty() {
                Vec::new()
            } else {
                vec![resolve_host_choice(raw, &question.options)?]
            }
        }
        umadev_runtime::HostQuestionKind::MultiChoice => {
            let tokens = raw
                .split([',', '，', ';', '；', '\n'])
                .map(str::trim)
                .filter(|token| !token.is_empty());
            let mut values = Vec::new();
            for token in tokens {
                let value = resolve_host_choice(token, &question.options)?;
                if !values.contains(&value) {
                    values.push(value);
                }
            }
            values
        }
        umadev_runtime::HostQuestionKind::Confirmation => {
            if question.options.is_empty() {
                let value = match raw.to_ascii_lowercase().as_str() {
                    "y" | "yes" | "true" | "1" | "是" | "确认" | "確認" | "同意" => "yes",
                    "n" | "no" | "false" | "0" | "否" | "拒绝" | "拒絕" | "不同意" => "no",
                    _ => return Err("confirmation expects yes or no".to_string()),
                };
                vec![value.to_string()]
            } else {
                vec![resolve_host_choice(raw, &question.options)?]
            }
        }
    };
    if question.required && values.is_empty() {
        return Err(format!("`{}` requires an answer", question.id));
    }
    Ok(umadev_runtime::HostAnswer {
        question_id: question.id.clone(),
        values,
    })
}

pub(super) fn json_answer_text(value: &serde_json::Value) -> std::result::Result<String, String> {
    match value {
        serde_json::Value::String(value) => Ok(value.clone()),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "answer arrays must contain strings".to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(|values| values.join(",")),
        serde_json::Value::Null => Ok(String::new()),
        _ => Err("each structured answer must be a string or string array".to_string()),
    }
}

pub(super) fn parse_user_input_response(
    questions: &[umadev_runtime::HostQuestion],
    raw: &str,
) -> std::result::Result<umadev_runtime::HostResponse, String> {
    if questions.is_empty() {
        return Err("the base supplied no questions".to_string());
    }
    if questions.len() == 1 {
        return Ok(umadev_runtime::HostResponse::UserInput {
            answers: vec![parse_host_question_answer(&questions[0], raw)?],
        });
    }

    let answer_texts = if raw.trim_start().starts_with('{') {
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(raw).map_err(|error| format!("invalid answer JSON: {error}"))?;
        questions
            .iter()
            .map(|question| {
                object
                    .get(&question.id)
                    .map(json_answer_text)
                    .transpose()
                    .map(Option::unwrap_or_default)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        let lines = raw.lines().map(str::trim).collect::<Vec<_>>();
        if lines.len() != questions.len() {
            return Err(format!(
                "expected {} answer lines or a JSON object keyed by question id",
                questions.len()
            ));
        }
        lines.iter().map(|line| (*line).to_string()).collect()
    };

    let answers = questions
        .iter()
        .zip(answer_texts)
        .map(|(question, answer)| parse_host_question_answer(question, &answer))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(umadev_runtime::HostResponse::UserInput { answers })
}

pub(super) fn schema_accepts_top_level(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> bool {
    match schema.get("type").and_then(serde_json::Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_i64().is_some() || value.as_u64().is_some(),
        Some("number") => value.is_number(),
        Some("null") => value.is_null(),
        Some(_) | None => true,
    }
}

pub(super) fn parse_host_input_response(
    request: &umadev_runtime::HostRequest,
    raw: &str,
) -> std::result::Result<umadev_runtime::HostResponse, String> {
    if is_host_cancel_text(raw) {
        return Ok(umadev_runtime::HostResponse::Cancelled {
            reason: Some("cancelled by user".to_string()),
        });
    }
    match request {
        umadev_runtime::HostRequest::UserInput { questions, .. } => {
            parse_user_input_response(questions, raw)
        }
        umadev_runtime::HostRequest::McpElicitation {
            requested_schema, ..
        } => {
            let expected = requested_schema
                .get("type")
                .and_then(serde_json::Value::as_str);
            let content = match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(value) => value,
                Err(_) if expected == Some("string") => serde_json::Value::String(raw.to_string()),
                Err(error) => return Err(format!("invalid JSON response: {error}")),
            };
            if !schema_accepts_top_level(requested_schema, &content) {
                return Err(format!(
                    "response does not match requested top-level type `{}`",
                    expected.unwrap_or("JSON")
                ));
            }
            Ok(umadev_runtime::HostResponse::McpElicitation {
                action: umadev_runtime::HostElicitationAction::Accept,
                content: Some(content),
            })
        }
        _ => Err("request does not accept free-form host input".to_string()),
    }
}

/// Register an interactive request and wait without ending the base turn. The
/// request is bounded to the same five-minute budget as approvals; every dropped
/// channel/timeout returns a protocol-shaped safe rejection.
pub(super) async fn await_host_input(
    holder: &HostInputHolder,
    sink: &Arc<ChannelSink>,
    request: &umadev_runtime::HostRequest,
    req_id: &str,
) -> umadev_runtime::HostResponse {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let token = NEXT_HOST_INPUT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match holder.lock() {
        Ok(mut guard) => {
            // An older request still parked here means the serially-asking base has
            // ABANDONED it: it stopped waiting (its own timeout / it answered itself)
            // and moved on — this NEW ask is the live one. The old behaviour rejected
            // the NEWCOMER ("another host response is already pending"), which bricked
            // every later interactive ask behind one stale occupant: each terminal /
            // permission question came back to the base as an instant rejection
            // (rendered as "User rejected the execution"), commands stopped running,
            // and the whole pipeline fail-closed — while the dead picker stayed on
            // screen. Supersede instead: settle the stale RPC with a protocol-shaped
            // cancel (harmless if the server already dropped it) and register the
            // live request; the per-frame UI sync then swaps the picker.
            if let Some(stale) = guard.take() {
                let _ = stale
                    .reply_tx
                    .send(umadev_runtime::HostResponse::Cancelled {
                        reason: Some(
                            "superseded by a newer interactive request from the base".to_string(),
                        ),
                    });
                sink.emit(EngineEvent::Note(
                    "[note] the base moved on to a new question; the earlier unanswered one was cancelled"
                        .to_string(),
                ));
            }
            *guard = Some(PendingHostInput {
                token,
                req_id: req_id.to_string(),
                reply_tx: tx,
                request: request.clone(),
            });
        }
        Err(_) => return request.safe_rejection("host response bridge unavailable"),
    }
    sink.emit(EngineEvent::Note(host_request_note(request)));
    let response = match tokio::time::timeout(APPROVAL_WAIT_BUDGET, rx).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => request.safe_rejection("host response was cancelled"),
        Err(_) => {
            sink.emit(EngineEvent::Note(
                "[warn] host response timed out and was safely rejected".to_string(),
            ));
            // The reason text reaches the BASE, and a bare "host response timed
            // out" was echoed back as if it were the user's words ("The user
            // said: host response timed out") and treated as revision feedback.
            // Say explicitly that no user input arrived and it carries no intent.
            request.safe_rejection(
                "no user answer arrived within the wait window; this is not user feedback — \
                 re-ask when needed or proceed with your best judgment",
            )
        }
    };
    // The UI takes the slot before sending a response. If a later request was
    // registered in the tiny wake-up window, never let this older waiter clear
    // the newer RPC.
    clear_pending_host_input_if(holder, token);
    response
}

/// Route Enter/Esc to a live typed request before ordinary chat submission. A
/// validation error consumes Enter but preserves the draft; all other keys keep
/// flowing through the normal UTF-8 editor (including Shift+Enter newlines).
pub(super) fn resolve_pending_host_input_key(
    holder: &HostInputHolder,
    app: &mut App,
    sink: &Arc<ChannelSink>,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> bool {
    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
        return false;
    }
    let pending = holder.lock().ok().is_some_and(|guard| guard.is_some());
    if !pending {
        return false;
    }
    if let Some(outcome) = app.resolve_specialized_host_input_key(code, modifiers) {
        match outcome {
            HostInputKeyOutcome::Passthrough => return false,
            HostInputKeyOutcome::Consumed => return true,
            HostInputKeyOutcome::Invalid(error) => {
                sink.emit(EngineEvent::Note(format!(
                    "[warn] invalid host response: {error}"
                )));
                return true;
            }
            HostInputKeyOutcome::Respond { token, response } => {
                let taken = holder.lock().ok().and_then(|mut guard| {
                    guard
                        .as_ref()
                        .is_some_and(|pending| pending.token == token)
                        .then(|| guard.take())
                        .flatten()
                });
                let Some(pending) = taken else {
                    return true;
                };
                let plan_approved = matches!(
                    &response,
                    umadev_runtime::HostResponse::PlanOutcome {
                        outcome: umadev_runtime::HostPlanOutcome::Approved
                    }
                );
                let _ = pending.reply_tx.send(response);
                app.set_pending_host_input(None);
                // Approving the plan review under the read-only Plan tier is the
                // user's authorization to IMPLEMENT — promote to the writable
                // Guarded tier (see `promote_plan_approval_to_execution`).
                if plan_approved {
                    app.promote_plan_approval_to_execution();
                }
                return true;
            }
        }
    }
    if code == KeyCode::Esc && modifiers.is_empty() {
        let secret = holder
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .map(|pending| host_request_is_secret(&pending.request))
            })
            .unwrap_or(false);
        if let Ok(mut guard) = holder.lock() {
            if let Some(pending) = guard.take() {
                let _ = pending
                    .reply_tx
                    .send(umadev_runtime::HostResponse::Cancelled {
                        reason: Some("cancelled by user".to_string()),
                    });
            }
        }
        app.discard_host_response_input(secret);
        return true;
    }
    if code != KeyCode::Enter || modifiers.contains(KeyModifiers::SHIFT) {
        return false;
    }

    let raw = app.prepared_host_response_input();
    let parsed = holder.lock().ok().and_then(|guard| {
        guard
            .as_ref()
            .map(|pending| parse_host_input_response(&pending.request, &raw))
    });
    let Some(parsed) = parsed else {
        return false;
    };
    let response = match parsed {
        Ok(response) => response,
        Err(error) => {
            sink.emit(EngineEvent::Note(format!(
                "[warn] invalid host response: {error}; edit the answer and press Enter again"
            )));
            return true;
        }
    };
    let secret = holder
        .lock()
        .ok()
        .and_then(|guard| {
            guard
                .as_ref()
                .map(|pending| host_request_is_secret(&pending.request))
        })
        .unwrap_or(false);
    if let Ok(mut guard) = holder.lock() {
        if let Some(pending) = guard.take() {
            let _ = pending.reply_tx.send(response);
            let _ = app.accept_host_response_input(secret);
            return true;
        }
    }
    false
}

/// Upper bound on how long an interactive guarded approval blocks the drain waiting
/// for the user, after which it fail-open DENIES (safe: the base just doesn't run that
/// action) and surfaces a note. Generous — a present user answers in seconds — but
/// bounded so a walked-away user can never hold the resident session open forever.
pub(super) const APPROVAL_WAIT_BUDGET: Duration = Duration::from_secs(300);

/// Process-global LIVE trust tier so a MID-TURN mode switch (shift+Tab / `/mode` /
/// `/auto` / `/manual`) takes effect on the IN-FLIGHT chat turn — not just the snapshot
/// captured when the turn was spawned. Reported bug: a user sent a command in Guarded,
/// then switched to Auto to unblock a paused edit, but the running turn kept denying
/// because it still ran under the spawn-time Guarded snapshot. The event loop republishes
/// this on every mode change; the resident chat drain reads it at each approval decision.
/// Encoded 0=Plan, 1=Guarded, 2=Auto. One TUI session per process, so a single global is
/// the entire state.
pub(super) static LIVE_TRUST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Encode a [`TrustMode`] for [`LIVE_TRUST`].
pub(super) fn trust_to_u8(m: umadev_agent::TrustMode) -> u8 {
    match m {
        umadev_agent::TrustMode::Plan => 0,
        umadev_agent::TrustMode::Guarded => 1,
        umadev_agent::TrustMode::Auto => 2,
    }
}

/// Decode a [`LIVE_TRUST`] byte back to a [`TrustMode`] (unknown → the safe Guarded).
pub(super) fn trust_from_u8(v: u8) -> umadev_agent::TrustMode {
    match v {
        0 => umadev_agent::TrustMode::Plan,
        2 => umadev_agent::TrustMode::Auto,
        _ => umadev_agent::TrustMode::Guarded,
    }
}

/// Publish the current effective trust tier so the in-flight drain sees mode switches
/// live. Called by the event loop whenever the mode could have changed.
pub(super) fn publish_live_trust(m: umadev_agent::TrustMode) {
    LIVE_TRUST.store(trust_to_u8(m), std::sync::atomic::Ordering::Relaxed);
}

/// The LIVE trust tier — what the resident chat drain reads at each approval decision so
/// a mid-turn switch applies to the turn already running.
pub(super) fn live_trust_tier() -> umadev_agent::TrustMode {
    trust_from_u8(LIVE_TRUST.load(std::sync::atomic::Ordering::Relaxed))
}

/// Trust read used by a resident turn. Production follows the live TUI switch;
/// unit turns use their explicit snapshot so parallel tests cannot contaminate
/// one another through the process-global event-loop bridge.
pub(super) fn trust_for_resident_turn(
    snapshot: umadev_agent::TrustMode,
) -> umadev_agent::TrustMode {
    #[cfg(test)]
    {
        snapshot
    }
    #[cfg(not(test))]
    {
        let _ = snapshot;
        live_trust_tier()
    }
}

/// Whether a live user is present at an interactive terminal — the `has_user` /
/// `interactive` signal threaded into the pause decisions. The TUI event loop only
/// runs under a real TTY (raw mode is on), so this is `true` in normal use and `false`
/// for a piped / non-TTY invocation — in which case the pauses stay OFF and the turn
/// keeps today's headless auto-decide behaviour (fail-open toward never-blocking).
pub(super) fn interactive_user_present() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Event-loop hook (runs on the UI thread, before the normal key→`Action` pipeline):
/// if the resident chat drain is BLOCKED on a guarded approval, an EMPTY-input
/// `y`/`n`/Esc keypress IS the decision: consume it so it can never leak into the
/// input line. Returns `true` when the key was consumed (the caller then skips the
/// normal action dispatch for it).
///
/// - No approval pending → returns `false` immediately (the key flows normally).
/// - A **modified** key (Ctrl-C cancel, Ctrl-O, …) is NEVER intercepted, so hard-cancel
///   still works mid-pause.
/// - Esc → [`ApprovalReply::Deny`] always (the advertised deny key — kept even with
///   text in the box so it can never fall through to the interrupt/quit gesture
///   mid-pause and nuke the whole run).
/// - With an EMPTY input line: `y`/`Y` → [`ApprovalReply::Allow`]; `n`/`N` →
///   [`ApprovalReply::Deny`].
/// - **Every other key flows through** (A2#5): the old behaviour swallowed every bare
///   key, so a user typing 「批准」 saw dead keys and had no approval entry point at
///   all. Now characters land in the input line and `App::submit_text` classifies the
///   submitted text (「批准」/"approve" → allow, 「拒绝」/"deny" → deny) via
///   [`crate::app::Action::ApprovalReply`]. A stray Enter on an empty box is a no-op
///   submit, and a non-approval submit parks on the normal queued-chat / steering
///   lanes — a paused session can still never grow a second concurrent turn.
///
/// Fail-open: a poisoned lock returns `false` (the key flows normally, nothing hangs).
pub(super) fn resolve_pending_approval(
    holder: &ApprovalHolder,
    code: KeyCode,
    mods: KeyModifiers,
    input_empty: bool,
) -> bool {
    // A modified chord (Ctrl-C / Alt-… / Super-…) is left for the normal pipeline so the
    // user can always hard-cancel the paused turn. A bare Shift is still "unmodified".
    if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
        return false;
    }
    let Ok(mut guard) = holder.lock() else {
        return false;
    };
    if guard.is_none() {
        return false; // no pause active — the key flows through untouched
    }
    // shift+Tab (BackTab) must FALL THROUGH even while a pause is active so it reaches the
    // trust-mode cycle (`cycle_approval_mode`): the advertised "shift+Tab 转手动 / flip to
    // Auto to release the paused action" only works if this keystroke is NOT swallowed
    // here. The mode-cycle handler then republishes the live tier and, when it lands on
    // Auto, RELEASES this pending approval as Allow — unless the narrowed Auto floor
    // still escalates it (see `release_pending_approval_on_auto_switch` after
    // `apply_key_with_mods`; a true disaster keeps its explicit prompt).
    if matches!(code, KeyCode::BackTab) {
        return false;
    }
    let decision = match code {
        // Esc denies regardless of input content: it is the advertised deny key, and
        // letting it fall through with text in the box would reach the Esc interrupt
        // arm (`is_pipeline_active`) — a double-Esc there cancels the WHOLE run.
        KeyCode::Esc => Some(ApprovalReply::Deny),
        KeyCode::Char('y' | 'Y') if input_empty => Some(ApprovalReply::Allow),
        KeyCode::Char('n' | 'N') if input_empty => Some(ApprovalReply::Deny),
        _ => None,
    };
    if let Some(d) = decision {
        if let Some(p) = guard.take() {
            let _ = p.reply_tx.send(d); // a dropped receiver (task gone) is harmless
        }
        return true;
    }
    // Everything else flows into the normal pipeline: the user can TYPE a reply
    // (「批准」/「拒绝」, classified at submit) instead of facing dead keys.
    false
}

/// Whether a base `NeedApproval` should PAUSE for the live user rather than
/// auto-decide on the floor. Two lanes:
/// - **Guarded per-item review** ([`umadev_agent::guarded_should_pause_item`]) —
///   a consequential, un-remembered action under Guarded with a live user.
/// - **AUTO residual escalation** — a TRUE disaster the narrowed Auto floor
///   still confirms (`rm -rf`, a force-push, credential exfiltration, an
///   out-of-tree write). With a live user present it must SURFACE the visible
///   prompt, never headless-deny (the reported "待批准 with no entry, had to
///   drop to the raw CLI"). Headless Auto keeps the deterministic deny floor.
///
/// Pure + deterministic (unit-tested without the process-global trust tier).
pub(super) fn should_pause_for_user(
    mode: umadev_agent::TrustMode,
    interactive: bool,
    cap: umadev_agent::Capability,
    already_remembered: bool,
    needs_confirm: bool,
) -> bool {
    umadev_agent::guarded_should_pause_item(mode, interactive, interactive, cap, already_remembered)
        || (needs_confirm && interactive && matches!(mode, umadev_agent::TrustMode::Auto))
}

/// Snapshot the in-flight approval pause's `(action, target)` for the app model —
/// the renderer pins these into the sticky approval bar above the input box.
/// Fail-open: a poisoned lock / no pause reads as `None` (bar hidden).
pub(super) fn pending_approval_item(holder: &ApprovalHolder) -> Option<(String, String)> {
    holder
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|p| (p.action.clone(), p.target.clone())))
}

/// Resolve an in-flight guarded approval as DENY — the typed-reply path
/// (「拒绝」/"deny" submitted while the pause is active). Fail-open: a poisoned
/// lock / no pending approval is a no-op (the drain's own budget still bounds it).
pub(super) fn deny_pending_approval(holder: &ApprovalHolder) {
    if let Ok(mut g) = holder.lock() {
        if let Some(p) = g.take() {
            let _ = p.reply_tx.send(ApprovalReply::Deny);
        }
    }
}

/// Clear any pending approval (dropping its `reply_tx` so the drain's `await` fail-opens
/// to DENY). Called when a turn is cancelled / a terminal decision lands, so a stale
/// wait can never linger. Fail-open on a poisoned lock (nothing to clear / no hang).
pub(super) fn clear_pending_approval(holder: &ApprovalHolder) {
    if let Ok(mut g) = holder.lock() {
        *g = None;
    }
}

/// Resolve an in-flight guarded approval as ALLOW — the user's EXPLICIT verdict
/// (a typed 「批准」/"approve" via [`crate::app::Action::ApprovalReply`], or the
/// empty-input `y` key). Always resolves, whatever the item: an explicit human
/// approval is exactly what the prompt asked for. Fail-open: a poisoned lock /
/// no pending approval is a no-op.
pub(super) fn allow_pending_approval(holder: &ApprovalHolder) {
    if let Ok(mut g) = holder.lock() {
        if let Some(p) = g.take() {
            let _ = p.reply_tx.send(ApprovalReply::Allow);
        }
    }
}

/// Release an in-flight approval on a MODE SWITCH to Auto (shift+Tab / `/mode`
/// mid-turn): the currently-paused action proceeds immediately instead of
/// waiting out [`APPROVAL_WAIT_BUDGET`] and fail-open DENYing — which is exactly
/// the reported "switched to Auto but the edit was still rejected".
///
/// **Floor guard — this is NOT an explicit approval:** an item the narrowed AUTO
/// floor would STILL escalate (a true disaster — `rm -rf`, a force-push,
/// credential exfiltration, an out-of-tree write; see
/// [`umadev_agent::floor_escalates`]) is NOT silently released by the mode
/// switch: it stays pending so the user answers the visible prompt explicitly
/// (typed 「批准」 / `y` still resolves it via [`allow_pending_approval`]). An
/// ordinary item (an npm install, an in-tree write) resolves Allow, matching the
/// tier the user just opted into. Fail-open: a poisoned lock / no pending
/// approval is a no-op.
pub(super) fn release_pending_approval_on_auto_switch(holder: &ApprovalHolder) {
    if let Ok(mut g) = holder.lock() {
        if g.as_ref().is_some_and(|p| !p.auto_releasable) {
            return; // the base's effective policy still requires a human answer
        }
        let still_escalates = g.as_ref().is_some_and(|p| {
            umadev_agent::requires_confirmation(umadev_agent::TrustMode::Auto, &p.action, &p.target)
        });
        if still_escalates {
            return; // a true disaster keeps its explicit prompt even in Auto
        }
        if let Some(p) = g.take() {
            let _ = p.reply_tx.send(ApprovalReply::Allow);
        }
    }
}

/// INTERACTIVE pause (Fix ③): register a [`PendingApproval`], surface the item, and
/// block until the user answers — bounded by [`APPROVAL_WAIT_BUDGET`] and cancellable
/// (Esc / a cleared holder). Returns the user's [`ApprovalReply`], failing open to
/// [`ApprovalReply::Deny`] on EVERY error path (can't register, the channel dropped, or
/// the budget elapsed) so the base is never left hanging and the drain never wedges.
pub(super) async fn await_user_approval(
    holder: &ApprovalHolder,
    sink: &Arc<ChannelSink>,
    action: &str,
    target: &str,
) -> ApprovalReply {
    // Callers with no base RPC (host-git, a Guarded local pause, the /run
    // callback) carry no `req_id` — a HostRequestSettled can never correlate to
    // these, so an empty id is correct.
    await_user_approval_with_auto_release(holder, sink, action, target, true, "").await
}

/// The `HostRequest::Approval` variant: carries the base `req_id` so a later
/// `HostRequestSettled` can retract exactly this approval bar.
pub(super) async fn await_user_approval_for_request(
    holder: &ApprovalHolder,
    sink: &Arc<ChannelSink>,
    action: &str,
    target: &str,
    req_id: &str,
) -> ApprovalReply {
    await_user_approval_with_auto_release(holder, sink, action, target, true, req_id).await
}

/// Register the same visible approval pause with an explicit mode-switch
/// policy. Upstream-forced permission requests pass `false`, ensuring a local
/// switch to Auto cannot override the base's effective policy boundary.
async fn await_user_approval_with_auto_release(
    holder: &ApprovalHolder,
    sink: &Arc<ChannelSink>,
    action: &str,
    target: &str,
    auto_releasable: bool,
    req_id: &str,
) -> ApprovalReply {
    let (tx, rx) = tokio::sync::oneshot::channel();
    // Register the pause so the event loop routes the user's keypress here — carrying
    // the item's identity so the loop mirrors it into the sticky approval bar (A2#5).
    // If the lock is poisoned we can't register → fail-open DENY (never block on an
    // unroutable wait).
    match holder.lock() {
        Ok(mut g) => {
            // Supersede, never silently drop: overwriting a still-parked older
            // approval used to close its channel, so that waiter resolved to a
            // SILENT fail-open DENY — the base saw "User rejected" for an ask
            // the user never even saw (the approval twin of the host-input
            // supersede). The newest ask from a serially-asking base is the
            // live one; say the old one was superseded so a rejection the base
            // reports is never a mystery.
            if g.take().is_some() {
                sink.emit(EngineEvent::Note(
                    "[note] a newer approval request superseded the earlier unanswered one (it was declined safely)"
                        .to_string(),
                ));
            }
            *g = Some(PendingApproval {
                reply_tx: tx,
                action: action.to_string(),
                target: target.to_string(),
                auto_releasable,
                req_id: req_id.to_string(),
            });
        }
        Err(_) => return ApprovalReply::Deny,
    }
    sink.emit(EngineEvent::Note(umadev_i18n::tlf(
        "trust.pause.approve",
        &[action, target],
    )));
    // Bounded wait. A dropped sender (cancel / quit / a cleared holder / a dead session)
    // resolves the inner `rx` to `Err` → DENY; the outer timeout is the walked-away-user
    // backstop → DENY. Either way the drain resumes promptly and never hangs.
    let reply = match tokio::time::timeout(APPROVAL_WAIT_BUDGET, rx).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) => ApprovalReply::Deny, // channel dropped → fail-open deny
        Err(_) => {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.pause.timeout",
                &[action, target],
            )));
            ApprovalReply::Deny
        }
    };
    clear_pending_approval(holder);
    reply
}

fn is_upstream_permission_boundary(metadata: &serde_json::Value) -> bool {
    metadata
        .get("upstreamPermissionBoundary")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// Resolve a permission request that the base emitted even though UmaDev asked
/// for Auto. Its presence proves the effective upstream policy still requires
/// confirmation, so local trust-ledger memory and Auto release are inapplicable.
async fn resolve_upstream_permission_boundary(
    action: &str,
    target: &str,
    interactive: bool,
    approval_holder: &ApprovalHolder,
    sink: &Arc<ChannelSink>,
) -> umadev_runtime::ApprovalDecision {
    if !interactive {
        sink.emit(EngineEvent::Note(umadev_i18n::tlf(
            "continuous.dangerous_action_denied",
            &[action, target],
        )));
        return umadev_runtime::ApprovalDecision::Deny;
    }

    match await_user_approval_with_auto_release(approval_holder, sink, action, target, false, "")
        .await
    {
        ApprovalReply::Allow => {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.pause.allowed",
                &[action, target],
            )));
            umadev_runtime::ApprovalDecision::Allow
        }
        ApprovalReply::Deny => {
            sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                "trust.pause.denied",
                &[action, target],
            )));
            umadev_runtime::ApprovalDecision::Deny
        }
    }
}

pub(super) fn host_approval_option_id(
    options: &[umadev_runtime::HostApprovalOption],
    decision: umadev_runtime::ApprovalDecision,
) -> Option<String> {
    use umadev_runtime::HostApprovalOptionKind::{AllowOnce, RejectOnce};
    // A binary UmaDev approval is scoped to this request. It must never be
    // widened into a vendor's persistent AllowAlways/RejectAlways option merely
    // because the peer omitted its one-shot counterpart. Persistent choices
    // require an explicit picker that returns the exact vendor option id.
    let kind = match decision {
        umadev_runtime::ApprovalDecision::Allow => AllowOnce,
        umadev_runtime::ApprovalDecision::Deny => RejectOnce,
    };
    options
        .iter()
        .find(|option| option.kind == kind)
        .map(|option| option.id.clone())
}

/// Resolve one approval-shaped request through the live trust tier and ledger.
/// This is shared by legacy `NeedApproval` and typed host requests semantically;
/// the latter additionally preserves the selected protocol option id.
#[allow(clippy::too_many_arguments)] // a protocol resolver: each arg is a distinct, irreducible input
pub(super) async fn resident_approval_decision(
    action: &str,
    target: &str,
    req_id: &str,
    project_root: &std::path::Path,
    mode_snapshot: umadev_agent::TrustMode,
    interactive: bool,
    approval_holder: &ApprovalHolder,
    sink: &Arc<ChannelSink>,
) -> umadev_runtime::ApprovalDecision {
    let mode = trust_for_resident_turn(mode_snapshot);
    let cap = umadev_agent::capability_class(action, target);
    let ledger = umadev_agent::TrustLedger::load(project_root);
    let already = ledger.remembers_rooted(action, target, project_root);
    let needs_confirm = umadev_agent::requires_confirmation_with_ledger(
        mode,
        action,
        target,
        project_root,
        &ledger,
    );
    if should_pause_for_user(mode, interactive, cap, already, needs_confirm) {
        // Carry the base `req_id` (when this approval came from a
        // `HostRequest::Approval`) so a HostRequestSettled can retract the
        // approval bar too — not just the picker.
        match await_user_approval_for_request(approval_holder, sink, action, target, req_id).await {
            ApprovalReply::Allow => {
                umadev_agent::remember_project_approval(project_root, action, target);
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "trust.pause.allowed",
                    &[action, target],
                )));
                umadev_runtime::ApprovalDecision::Allow
            }
            ApprovalReply::Deny => {
                sink.emit(EngineEvent::Note(umadev_i18n::tlf(
                    "trust.pause.denied",
                    &[action, target],
                )));
                umadev_runtime::ApprovalDecision::Deny
            }
        }
    } else if needs_confirm {
        // The remediation must match the tier. Under Plan (read-only) this
        // branch is the common case, and the generic message pointed at
        // `UMADEV_CLAUDE_PERMISSION_MODE=bypassPermissions` — which is IGNORED in
        // Plan (it is read only in the Auto arm), so following it changed nothing
        // (the reported misleading advice). Plan gets a tier-correct message
        // pointing at `/mode guarded`; other tiers keep the existing text.
        let key = if mode == umadev_agent::TrustMode::Plan {
            "continuous.dangerous_action_denied_plan"
        } else {
            "continuous.dangerous_action_denied"
        };
        sink.emit(EngineEvent::Note(umadev_i18n::tlf(key, &[action, target])));
        umadev_runtime::ApprovalDecision::Deny
    } else {
        umadev_runtime::ApprovalDecision::Allow
    }
}

#[allow(clippy::too_many_arguments)] // a protocol resolver: each arg is a distinct, irreducible input
pub(super) async fn resolve_resident_host_request(
    request: &umadev_runtime::HostRequest,
    req_id: &str,
    project_root: &std::path::Path,
    mode: umadev_agent::TrustMode,
    interactive: bool,
    approval_holder: &ApprovalHolder,
    host_input_holder: &HostInputHolder,
    sink: &Arc<ChannelSink>,
) -> umadev_runtime::HostResponse {
    match request {
        umadev_runtime::HostRequest::Approval {
            action,
            target,
            options,
            metadata,
            ..
        } => {
            let decision = if is_upstream_permission_boundary(metadata) {
                resolve_upstream_permission_boundary(
                    action,
                    target,
                    interactive,
                    approval_holder,
                    sink,
                )
                .await
            } else {
                resident_approval_decision(
                    action,
                    target,
                    req_id,
                    project_root,
                    mode,
                    interactive,
                    approval_holder,
                    sink,
                )
                .await
            };
            umadev_runtime::HostResponse::Approval {
                selected_option_id: host_approval_option_id(options, decision),
                decision,
                message: matches!(decision, umadev_runtime::ApprovalDecision::Deny)
                    .then(|| "denied by user or UmaDev trust policy".to_string()),
            }
        }
        umadev_runtime::HostRequest::PermissionExpansion {
            permissions,
            reason,
            ..
        } => {
            let target = permissions
                .iter()
                .map(|permission| {
                    permission.target.as_ref().map_or_else(
                        || permission.kind.clone(),
                        |target| format!("{}:{target}", permission.kind),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sink.emit(EngineEvent::Note(format!(
                "[permission] {}{}",
                reason
                    .as_deref()
                    .unwrap_or("the base requested additional access"),
                if target.is_empty() {
                    String::new()
                } else {
                    format!("\n{target}")
                }
            )));
            let decision = resident_approval_decision(
                "permission-expansion",
                &target,
                req_id,
                project_root,
                mode,
                interactive,
                approval_holder,
                sink,
            )
            .await;
            umadev_runtime::HostResponse::PermissionExpansion {
                decision,
                granted: matches!(decision, umadev_runtime::ApprovalDecision::Allow)
                    .then(|| permissions.clone())
                    .unwrap_or_default(),
                message: reason.clone(),
            }
        }
        umadev_runtime::HostRequest::PlanConfirmation { metadata, .. }
            if metadata
                .get("responseContract")
                .and_then(serde_json::Value::as_str)
                == Some("grok_exit_plan_mode_v1")
                && interactive =>
        {
            // 自动过门 keeps its word here too: under the Auto tier the base's own
            // plan-review gate is auto-APPROVED with a visible note, exactly like
            // every other gate. Leaving it interactive under Auto caused the
            // reported strand: the picker went unanswered (or was answered
            // invalidly), grok stayed in its sticky session-level PLAN MODE, and
            // every later execution came back "User rejected the execution" while
            // the footer promised full autonomy. Guarded/Plan keep the human
            // picker — reviewing the plan is exactly what those tiers are for.
            if trust_for_resident_turn(mode) == umadev_agent::TrustMode::Auto {
                sink.emit(EngineEvent::Note(
                    umadev_i18n::tl("plan.review.auto_approved").to_string(),
                ));
                umadev_runtime::HostResponse::PlanOutcome {
                    outcome: umadev_runtime::HostPlanOutcome::Approved,
                }
            } else {
                await_host_input(host_input_holder, sink, request, req_id).await
            }
        }
        umadev_runtime::HostRequest::PlanConfirmation { metadata, .. }
            if metadata
                .get("responseContract")
                .and_then(serde_json::Value::as_str)
                == Some("grok_exit_plan_mode_v1") =>
        {
            request.safe_rejection("interactive response unavailable")
        }
        umadev_runtime::HostRequest::PlanConfirmation { plan, message, .. } => {
            sink.emit(EngineEvent::Note(format!(
                "[plan] {}\n{}",
                message
                    .as_deref()
                    .unwrap_or("the base requests confirmation"),
                plan.chars().take(12_000).collect::<String>()
            )));
            let target = plan
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("plan");
            let decision = resident_approval_decision(
                "plan-confirmation",
                target,
                req_id,
                project_root,
                mode,
                interactive,
                approval_holder,
                sink,
            )
            .await;
            umadev_runtime::HostResponse::PlanConfirmation {
                decision,
                feedback: matches!(decision, umadev_runtime::ApprovalDecision::Deny).then(|| {
                    message
                        .clone()
                        .unwrap_or_else(|| "plan execution was not approved".to_string())
                }),
            }
        }
        umadev_runtime::HostRequest::UserInput { .. }
        | umadev_runtime::HostRequest::McpElicitation { .. }
        | umadev_runtime::HostRequest::FolderTrust { .. }
            if interactive =>
        {
            await_host_input(host_input_holder, sink, request, req_id).await
        }
        umadev_runtime::HostRequest::FolderTrust { .. } => {
            request.safe_rejection("interactive folder-trust confirmation is unavailable")
        }
        umadev_runtime::HostRequest::Unknown { method, .. } => {
            sink.emit(EngineEvent::Note(format!(
                "[warn] unsupported host request `{method}` was safely rejected"
            )));
            request.safe_rejection("unsupported host request")
        }
        umadev_runtime::HostRequest::UserInput { .. }
        | umadev_runtime::HostRequest::McpElicitation { .. } => {
            request.safe_rejection("interactive response unavailable")
        }
    }
}
