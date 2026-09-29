//! Redaction at the base-adapter boundary.
//!
//! What a base produces falls in two groups. Model output (text, thinking, a
//! proposed plan), tool calls with their inputs, tool output and results, and
//! the action and target of an approval request pass through unchanged: the
//! transcript shows them, governance scans them (the hardcoded-secret rule
//! needs the secret it is looking for), and trust decisions authorize exactly
//! what the base will run. Everything else a vendor sends, such as errors,
//! warnings, request metadata, model and command catalogs and background task
//! labels, is diagnostic text and has its secret values redacted here.
//! Anything that persists the first group redacts it where it is written.

use serde_json::Value;
use umadev_runtime::{
    BackgroundProcessInfo, BackgroundProcessSignal, BackgroundTaskSignal, HostApprovalOption,
    HostApprovalOptionKind, HostPermission, HostQuestion, HostQuestionKind, HostQuestionOption,
    HostRequest, SessionCommandInfo, SessionError, SessionEvent, SessionModelInfo,
    SessionPlanEntry, SessionReasoningEffortOption, SessionStateUpdate, StreamEvent, TurnStatus,
};

pub(crate) fn redact_text(text: &str) -> String {
    umadev_governance::redaction::redact_text(text)
}

pub(crate) fn sanitize_value(value: Value) -> Value {
    umadev_governance::redaction::redact_json(value)
}

pub(crate) fn sanitize_stream_event(event: StreamEvent) -> StreamEvent {
    match event {
        StreamEvent::Warning { message } => StreamEvent::Warning {
            message: redact_text(&message),
        },
        event => event,
    }
}

fn sanitize_approval_option(option: HostApprovalOption) -> HostApprovalOption {
    HostApprovalOption {
        id: redact_text(&option.id),
        label: redact_text(&option.label),
        kind: match option.kind {
            HostApprovalOptionKind::Other(kind) => {
                HostApprovalOptionKind::Other(redact_text(&kind))
            }
            kind => kind,
        },
    }
}

fn sanitize_question_option(option: HostQuestionOption) -> HostQuestionOption {
    HostQuestionOption {
        value: redact_text(&option.value),
        label: redact_text(&option.label),
        description: option.description.map(|value| redact_text(&value)),
        preview: option.preview.map(|value| redact_text(&value)),
    }
}

fn sanitize_question(question: HostQuestion) -> HostQuestion {
    HostQuestion {
        id: redact_text(&question.id),
        header: question.header.map(|value| redact_text(&value)),
        prompt: redact_text(&question.prompt),
        kind: match question.kind {
            HostQuestionKind::Other(kind) => HostQuestionKind::Other(redact_text(&kind)),
            kind => kind,
        },
        required: question.required,
        options: question
            .options
            .into_iter()
            .map(sanitize_question_option)
            .collect(),
    }
}

fn sanitize_permission(permission: HostPermission) -> HostPermission {
    HostPermission {
        kind: redact_text(&permission.kind),
        target: permission.target.map(|value| redact_text(&value)),
        metadata: sanitize_value(permission.metadata),
    }
}

fn sanitize_host_request(request: HostRequest) -> HostRequest {
    match request {
        HostRequest::Approval {
            action,
            target,
            message,
            options,
            metadata,
        } => HostRequest::Approval {
            action,
            target,
            message: message.map(|value| redact_text(&value)),
            options: options.into_iter().map(sanitize_approval_option).collect(),
            metadata: sanitize_value(metadata),
        },
        HostRequest::UserInput {
            questions,
            metadata,
        } => HostRequest::UserInput {
            questions: questions.into_iter().map(sanitize_question).collect(),
            metadata: sanitize_value(metadata),
        },
        HostRequest::PermissionExpansion {
            permissions,
            reason,
            metadata,
        } => HostRequest::PermissionExpansion {
            permissions: permissions.into_iter().map(sanitize_permission).collect(),
            reason: reason.map(|value| redact_text(&value)),
            metadata: sanitize_value(metadata),
        },
        HostRequest::McpElicitation {
            server_name,
            message,
            requested_schema,
            metadata,
        } => HostRequest::McpElicitation {
            server_name: server_name.map(|value| redact_text(&value)),
            message: redact_text(&message),
            requested_schema: sanitize_value(requested_schema),
            metadata: sanitize_value(metadata),
        },
        HostRequest::PlanConfirmation {
            plan,
            message,
            metadata,
        } => HostRequest::PlanConfirmation {
            plan,
            message: message.map(|value| redact_text(&value)),
            metadata: sanitize_value(metadata),
        },
        HostRequest::FolderTrust {
            cwd,
            workspace,
            config_kinds,
        } => HostRequest::FolderTrust {
            cwd: std::path::PathBuf::from(redact_text(&cwd.to_string_lossy())),
            workspace: std::path::PathBuf::from(redact_text(&workspace.to_string_lossy())),
            config_kinds: config_kinds
                .into_iter()
                .map(|kind| redact_text(&kind))
                .collect(),
        },
        HostRequest::Unknown { method, payload } => HostRequest::Unknown {
            method: redact_text(&method),
            payload: sanitize_value(payload),
        },
    }
}

fn sanitize_background_task(signal: BackgroundTaskSignal) -> BackgroundTaskSignal {
    match signal {
        BackgroundTaskSignal::Started { id } => BackgroundTaskSignal::Started {
            id: redact_text(&id),
        },
        BackgroundTaskSignal::Finished { id } => BackgroundTaskSignal::Finished {
            id: redact_text(&id),
        },
        BackgroundTaskSignal::Live { agent_ids } => BackgroundTaskSignal::Live {
            agent_ids: agent_ids.into_iter().map(|id| redact_text(&id)).collect(),
        },
    }
}

fn sanitize_background_process_info(process: BackgroundProcessInfo) -> BackgroundProcessInfo {
    BackgroundProcessInfo {
        task_id: redact_text(&process.task_id),
        tool_call_id: redact_text(&process.tool_call_id),
        kind: process.kind,
        description: process.description.map(|value| redact_text(&value)),
    }
}

fn sanitize_background_process(signal: BackgroundProcessSignal) -> BackgroundProcessSignal {
    match signal {
        BackgroundProcessSignal::Started { process } => BackgroundProcessSignal::Started {
            process: sanitize_background_process_info(process),
        },
        BackgroundProcessSignal::Finished {
            task_id,
            kind,
            exit_code,
            signal,
            truncated,
            will_wake,
        } => BackgroundProcessSignal::Finished {
            task_id: redact_text(&task_id),
            kind,
            exit_code,
            signal: signal.map(|value| redact_text(&value)),
            truncated,
            will_wake,
        },
        BackgroundProcessSignal::Live { processes } => BackgroundProcessSignal::Live {
            processes: processes
                .into_iter()
                .map(sanitize_background_process_info)
                .collect(),
        },
    }
}

fn sanitize_turn_status(status: TurnStatus) -> TurnStatus {
    match status {
        TurnStatus::Failed(reason) => TurnStatus::Failed(redact_text(&reason)),
        status => status,
    }
}

fn sanitize_reasoning_option(option: SessionReasoningEffortOption) -> SessionReasoningEffortOption {
    SessionReasoningEffortOption {
        id: redact_text(&option.id),
        value: option.value,
        label: redact_text(&option.label),
        description: option.description.map(|value| redact_text(&value)),
        default: option.default,
    }
}

fn sanitize_model_info(model: SessionModelInfo) -> SessionModelInfo {
    SessionModelInfo {
        model_id: redact_text(&model.model_id),
        name: redact_text(&model.name),
        description: model.description.map(|value| redact_text(&value)),
        total_context_tokens: model.total_context_tokens,
        agent_type: model.agent_type.map(|value| redact_text(&value)),
        supports_reasoning_effort: model.supports_reasoning_effort,
        reasoning_effort: model.reasoning_effort,
        reasoning_efforts: model
            .reasoning_efforts
            .into_iter()
            .map(sanitize_reasoning_option)
            .collect(),
    }
}

fn sanitize_command_info(command: SessionCommandInfo) -> SessionCommandInfo {
    SessionCommandInfo {
        name: redact_text(&command.name),
        description: redact_text(&command.description),
        input_hint: command.input_hint.map(|value| redact_text(&value)),
        scope: command.scope.map(|value| redact_text(&value)),
        source_path: command.source_path.map(|value| redact_text(&value)),
    }
}

fn sanitize_session_state_update(update: SessionStateUpdate) -> SessionStateUpdate {
    match update {
        SessionStateUpdate::ModelCatalogReplaced {
            current_model_id,
            available_models,
        } => SessionStateUpdate::ModelCatalogReplaced {
            current_model_id: redact_text(&current_model_id),
            available_models: available_models
                .into_iter()
                .map(sanitize_model_info)
                .collect(),
        },
        SessionStateUpdate::ModelChanged {
            model_id,
            reasoning_effort,
        } => SessionStateUpdate::ModelChanged {
            model_id: redact_text(&model_id),
            reasoning_effort,
        },
        SessionStateUpdate::ModelAutoSwitched {
            previous_model_id,
            new_model_id,
            reason,
        } => SessionStateUpdate::ModelAutoSwitched {
            previous_model_id: redact_text(&previous_model_id),
            new_model_id: redact_text(&new_model_id),
            reason: redact_text(&reason),
        },
        SessionStateUpdate::ModeChanged { mode } => SessionStateUpdate::ModeChanged { mode },
        SessionStateUpdate::ThinkingChanged {
            enabled,
            can_enable,
            can_disable,
        } => SessionStateUpdate::ThinkingChanged {
            enabled,
            can_enable,
            can_disable,
        },
        SessionStateUpdate::CommandCatalogReplaced { commands, tools } => {
            SessionStateUpdate::CommandCatalogReplaced {
                commands: commands.into_iter().map(sanitize_command_info).collect(),
                tools: tools.into_iter().map(|tool| redact_text(&tool)).collect(),
            }
        }
        SessionStateUpdate::PlanReplaced { entries } => SessionStateUpdate::PlanReplaced {
            entries: entries
                .into_iter()
                .map(|entry| SessionPlanEntry {
                    content: redact_text(&entry.content),
                    priority: entry.priority,
                    status: entry.status,
                })
                .collect(),
        },
    }
}

pub(crate) fn sanitize_session_event(event: SessionEvent) -> SessionEvent {
    match event {
        SessionEvent::SessionModel(model) => SessionEvent::SessionModel(redact_text(&model)),
        SessionEvent::StateUpdate(update) => {
            SessionEvent::StateUpdate(sanitize_session_state_update(update))
        }
        SessionEvent::HostRequest { req_id, request } => SessionEvent::HostRequest {
            req_id: redact_text(&req_id),
            request: sanitize_host_request(request),
        },
        SessionEvent::HostRequestSettled { req_id, reason } => SessionEvent::HostRequestSettled {
            req_id: redact_text(&req_id),
            reason: redact_text(&reason),
        },
        SessionEvent::BackgroundTask(signal) => {
            SessionEvent::BackgroundTask(sanitize_background_task(signal))
        }
        SessionEvent::BackgroundProcess(signal) => {
            SessionEvent::BackgroundProcess(sanitize_background_process(signal))
        }
        SessionEvent::PromptQueueChanged(mut snapshot) => {
            snapshot.session_id = redact_text(&snapshot.session_id);
            snapshot.running_prompt_id = snapshot
                .running_prompt_id
                .map(|prompt_id| redact_text(&prompt_id));
            for entry in &mut snapshot.entries {
                entry.id = redact_text(&entry.id);
                entry.owner = entry.owner.take().map(|owner| redact_text(&owner));
                entry.last_editor = entry.last_editor.take().map(|editor| redact_text(&editor));
                entry.kind = redact_text(&entry.kind);
                entry.text = redact_text(&entry.text);
            }
            SessionEvent::PromptQueueChanged(snapshot)
        }
        SessionEvent::TurnDone { status, usage } => SessionEvent::TurnDone {
            status: sanitize_turn_status(status),
            usage,
        },
        event => event,
    }
}

pub(crate) fn sanitize_session_error(error: SessionError) -> SessionError {
    match error {
        SessionError::Start(reason) => SessionError::Start(redact_text(&reason)),
        SessionError::Send(reason) => SessionError::Send(redact_text(&reason)),
        SessionError::InterruptPending(reason) => {
            SessionError::InterruptPending(redact_text(&reason))
        }
        SessionError::ForkUnsupported(reason) => {
            SessionError::ForkUnsupported(redact_text(&reason))
        }
        SessionError::CapabilityUnsupported(capability) => {
            SessionError::CapabilityUnsupported(capability)
        }
        SessionError::InputUnsupported {
            index,
            kind,
            reason,
        } => SessionError::InputUnsupported {
            index,
            kind,
            reason: redact_text(&reason),
        },
        SessionError::InputInvalid {
            index,
            kind,
            reason,
        } => SessionError::InputInvalid {
            index,
            kind,
            reason: redact_text(&reason),
        },
        SessionError::Closed => SessionError::Closed,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SECRET: &str = "SYNTH_SECRET_DO_NOT_LEAK_7f31";

    #[test]
    fn recursive_values_redact_secrets_but_keep_pagination_tokens() {
        let value = sanitize_value(json!({
            "authorization": format!("Bearer {SECRET}"),
            "nested": {"clientSecret": SECRET},
            "csrfToken": SECRET,
            "githubToken": SECRET,
            "npmToken": SECRET,
            "hfToken": SECRET,
            "webhookSecret": SECRET,
            "headers": {"x-private": SECRET},
            "nextPageToken": "page-2-safe",
            "cursor": "cursor-3-safe",
            "inputTokens": 42,
            "note": "pagination token and cursor are ordinary words"
        }));
        let rendered = value.to_string();
        assert!(!rendered.contains(SECRET));
        assert_eq!(value["nextPageToken"], "page-2-safe");
        assert_eq!(value["cursor"], "cursor-3-safe");
        assert_eq!(value["inputTokens"], 42);
        assert_eq!(
            value["note"],
            "pagination token and cursor are ordinary words"
        );
    }

    #[test]
    fn text_redaction_handles_assignments_bearer_prefixes_and_private_keys() {
        for text in [
            format!("Authorization: Bearer {SECRET}"),
            format!("OPENAI_API_KEY={SECRET}"),
            format!("GITHUB_TOKEN={SECRET}"),
            format!("Bearer {SECRET}"),
            format!("-----BEGIN PRIVATE KEY-----\n{SECRET}\n-----END PRIVATE KEY-----"),
        ] {
            let redacted = redact_text(&text);
            assert!(!redacted.contains(SECRET), "secret survived: {redacted}");
        }
        assert_eq!(
            redact_text("pagination token advances the cursor"),
            "pagination token advances the cursor"
        );
        assert_eq!(redact_text("PAGE_TOKEN=page-4"), "PAGE_TOKEN=page-4");
    }

    #[test]
    fn model_output_tool_traffic_and_approval_subjects_pass_through_unredacted() {
        let command = format!("export API_TOKEN={SECRET}; rm -rf ~");
        let events = [
            SessionEvent::TextDelta("interface User { password: string }".to_string()),
            SessionEvent::ToolCallCorrelated {
                call_id: "call-1".to_string(),
                name: "Write".to_string(),
                input: json!({
                    "file_path": "src/config.ts",
                    "content": format!("export const password = \"{SECRET}\";"),
                }),
            },
            SessionEvent::ToolResult {
                ok: true,
                summary: r#"{"nextPageToken":"x","password":"p"}"#.to_string(),
            },
            SessionEvent::NeedApproval {
                req_id: "r1".to_string(),
                action: "Bash".to_string(),
                target: command.clone(),
            },
            SessionEvent::HostRequest {
                req_id: "r2".to_string(),
                request: HostRequest::Approval {
                    action: "Bash".to_string(),
                    target: command,
                    message: None,
                    options: Vec::new(),
                    metadata: Value::Null,
                },
            },
        ];
        for event in events {
            let rendered = format!("{event:?}");
            assert_eq!(format!("{:?}", sanitize_session_event(event)), rendered);
        }
    }

    #[test]
    fn stream_text_is_kept_and_warnings_are_redacted() {
        let text = StreamEvent::Text {
            delta: format!("password: \"{SECRET}\""),
        };
        assert_eq!(sanitize_stream_event(text.clone()), text);
        let warning = sanitize_stream_event(StreamEvent::Warning {
            message: format!("GITHUB_TOKEN={SECRET}"),
        });
        assert!(!format!("{warning:?}").contains(SECRET));
    }

    #[test]
    fn background_process_metadata_is_redacted_without_path_fields() {
        let event = sanitize_session_event(SessionEvent::BackgroundProcess(
            BackgroundProcessSignal::Started {
                process: BackgroundProcessInfo {
                    task_id: format!("API_KEY={SECRET}"),
                    tool_call_id: "call-1".to_string(),
                    kind: umadev_runtime::BackgroundProcessKind::Bash,
                    description: Some(format!("Authorization: Bearer {SECRET}")),
                },
            },
        ));
        let rendered = format!("{event:?}");
        assert!(!rendered.contains(SECRET));
        assert!(!rendered.contains("output_file"));
        assert!(!rendered.contains("cwd"));
    }

    #[test]
    fn session_errors_redact_vendor_bodies_before_the_tui_sees_them() {
        let error = sanitize_session_error(SessionError::Start(format!(
            "handshake failed: {{\"githubToken\":\"{SECRET}\"}}"
        )));
        assert!(!error.to_string().contains(SECRET));
    }
}
