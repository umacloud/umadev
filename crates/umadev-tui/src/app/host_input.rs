//! Composer ownership while a base RPC waits for terminal input.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyModifiers};
use umadev_runtime::{
    HostAnswer, HostFolderTrustDecision, HostPlanOutcome, HostQuestion, HostQuestionAnnotation,
    HostQuestionKind, HostRequest, HostResponse, HostUserInputOutcome,
};
use unicode_width::UnicodeWidthChar;

use super::{App, ChatRole};

const GROK_QUESTION_CONTRACT: &str = "grok_ask_user_question_v1";
const GROK_PLAN_CONTRACT: &str = "grok_exit_plan_mode_v1";

/// Immutable bridge identity copied out of the in-flight request holder. The
/// event loop publishes this every tick; [`App::set_pending_host_input`] keeps
/// the interactive picker intact while the token is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostInputDescriptor {
    pub(crate) token: u64,
    pub(crate) request: HostRequest,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyModifiers};
    use umadev_agent::ChannelSink;

    use super::*;
    use crate::interaction_bridge::{
        pending_host_input_item, resolve_pending_host_input_key, HostInputHolder, PendingHostInput,
    };

    fn grok_question_request(mode: &str) -> umadev_runtime::HostRequest {
        umadev_runtime::HostRequest::UserInput {
            questions: vec![umadev_runtime::HostQuestion {
                id: "local-q1".to_string(),
                header: Some("Database".to_string()),
                prompt: "Which database?".to_string(),
                kind: umadev_runtime::HostQuestionKind::SingleChoice,
                required: true,
                options: vec![
                    umadev_runtime::HostQuestionOption {
                        value: "pg-id".to_string(),
                        label: "Postgres".to_string(),
                        description: Some("Relational".to_string()),
                        preview: Some("CREATE TABLE users (...)".to_string()),
                    },
                    umadev_runtime::HostQuestionOption {
                        value: "sqlite-id".to_string(),
                        label: "SQLite".to_string(),
                        description: Some("Embedded".to_string()),
                        preview: Some("PRAGMA foreign_keys = ON".to_string()),
                    },
                ],
            }],
            metadata: serde_json::json!({
                "responseContract":"grok_ask_user_question_v1",
                "mode":mode
            }),
        }
    }

    fn grok_plan_request(plan: &str) -> umadev_runtime::HostRequest {
        umadev_runtime::HostRequest::PlanConfirmation {
            plan: plan.to_string(),
            message: Some("Review before implementation".to_string()),
            metadata: serde_json::json!({
                "responseContract":"grok_exit_plan_mode_v1"
            }),
        }
    }

    fn install_host_request(
        app: &mut App,
        token: u64,
        request: umadev_runtime::HostRequest,
    ) -> (
        HostInputHolder,
        tokio::sync::oneshot::Receiver<umadev_runtime::HostResponse>,
    ) {
        let holder: HostInputHolder = Arc::new(std::sync::Mutex::new(None));
        let (tx, rx) = tokio::sync::oneshot::channel();
        *holder.lock().unwrap() = Some(PendingHostInput {
            token,
            reply_tx: tx,
            request,
        });
        app.set_pending_host_input(pending_host_input_item(&holder));
        (holder, rx)
    }

    fn host_key(holder: &HostInputHolder, app: &mut App, code: KeyCode) -> bool {
        let (sink, _events) = ChannelSink::new();
        resolve_pending_host_input_key(holder, app, &Arc::new(sink), code, KeyModifiers::NONE)
    }

    #[test]
    fn grok_question_picker_preserves_preview_notes_and_restores_chat_draft() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "grok-question-picker",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.input = "unrelated future chat".to_string();
        app.input_cursor = app.input.chars().count();
        let (holder, rx) = install_host_request(&mut app, 11, grok_question_request("plan"));

        let initial = app
            .pending_host_input
            .as_ref()
            .unwrap()
            .panel_lines(umadev_i18n::Lang::En)
            .join("\n");
        assert!(initial.contains("CREATE TABLE users"));
        assert!(initial.contains("chat about this"));

        assert!(host_key(&holder, &mut app, KeyCode::Down));
        assert!(host_key(&holder, &mut app, KeyCode::Char(' ')));
        assert!(host_key(&holder, &mut app, KeyCode::Tab));
        app.input = "Keep migrations reversible".to_string();
        app.input_cursor = app.input.chars().count();
        // First Esc exits notes and preserves them; it must not cancel the RPC.
        assert!(host_key(&holder, &mut app, KeyCode::Esc));
        assert!(holder.lock().unwrap().is_some());
        assert!(host_key(&holder, &mut app, KeyCode::Enter));

        let response = rx.blocking_recv().unwrap();
        let umadev_runtime::HostResponse::UserInputOutcome {
            outcome:
                umadev_runtime::HostUserInputOutcome::Accepted {
                    answers,
                    annotations,
                },
        } = response
        else {
            panic!("expected accepted Grok response");
        };
        assert_eq!(answers[0].question_id, "local-q1");
        assert_eq!(answers[0].values, ["sqlite-id"]);
        assert_eq!(
            annotations[0].preview.as_deref(),
            Some("PRAGMA foreign_keys = ON")
        );
        assert_eq!(
            annotations[0].notes.as_deref(),
            Some("Keep migrations reversible")
        );
        assert_eq!(app.input, "unrelated future chat");
    }

    #[test]
    fn grok_plan_question_chat_and_skip_keep_partial_other_without_notes_text() {
        for (token, action, expected_chat) in [(21, 'c', true), (22, 's', false)] {
            let tmp = tempfile::TempDir::new().unwrap();
            let mut app = App::new(
                "grok-question-partial",
                crate::config::UserConfig::default(),
                tmp.path().join("config.toml"),
                tmp.path().to_path_buf(),
            );
            let (holder, rx) = install_host_request(&mut app, token, grok_question_request("plan"));
            assert!(host_key(&holder, &mut app, KeyCode::Tab));
            app.input = "private note text".to_string();
            app.input_cursor = app.input.chars().count();
            assert!(host_key(&holder, &mut app, KeyCode::Esc));
            assert!(host_key(&holder, &mut app, KeyCode::Char(action)));
            let response = rx.blocking_recv().unwrap();
            let partial = match response {
                umadev_runtime::HostResponse::UserInputOutcome {
                    outcome: umadev_runtime::HostUserInputOutcome::ChatAboutThis { partial_answers },
                } if expected_chat => partial_answers,
                umadev_runtime::HostResponse::UserInputOutcome {
                    outcome: umadev_runtime::HostUserInputOutcome::SkipInterview { partial_answers },
                } if !expected_chat => partial_answers,
                other => panic!("unexpected partial outcome: {other:?}"),
            };
            assert_eq!(partial[0].values, ["Other"]);
            assert!(!format!("{partial:?}").contains("private note text"));
        }
    }

    #[test]
    fn grok_default_question_cannot_emit_plan_actions_and_escape_cancels_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "grok-question-default",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let (holder, mut rx) = install_host_request(&mut app, 23, grok_question_request("default"));
        assert!(host_key(&holder, &mut app, KeyCode::Char('c')));
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(host_key(&holder, &mut app, KeyCode::Esc));
        assert_eq!(
            rx.blocking_recv().unwrap(),
            umadev_runtime::HostResponse::UserInputOutcome {
                outcome: umadev_runtime::HostUserInputOutcome::Cancelled
            }
        );
        assert!(!host_key(&holder, &mut app, KeyCode::Esc));
    }

    #[test]
    fn approving_the_plan_under_the_read_only_tier_promotes_to_guarded_execution() {
        // "批准并开始实施" must actually lead to implementation: under the
        // read-only Plan tier the approval used to be a dead letter (writes
        // failed in the read-only sandbox, the next prompt re-pinned plan mode,
        // the run looped planning forever). Approving now promotes to Guarded —
        // the least-privilege WRITABLE tier — with a visible note; approving
        // while already writable changes nothing.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "plan-promote",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        app.trust_mode_override = Some(umadev_agent::TrustMode::Plan);
        let (holder, rx) = install_host_request(&mut app, 61, grok_plan_request("步骤一"));
        assert!(host_key(&holder, &mut app, KeyCode::Char('a')));
        assert!(matches!(
            rx.blocking_recv().unwrap(),
            umadev_runtime::HostResponse::PlanOutcome {
                outcome: umadev_runtime::HostPlanOutcome::Approved
            }
        ));
        assert_eq!(
            app.effective_trust_mode(),
            umadev_agent::TrustMode::Guarded,
            "plan approval under Plan promotes to the writable Guarded tier"
        );

        // Already-writable approval: no tier change.
        let tmp2 = tempfile::TempDir::new().unwrap();
        let mut auto_app = App::new(
            "plan-promote-auto",
            crate::config::UserConfig::default(),
            tmp2.path().join("config.toml"),
            tmp2.path().to_path_buf(),
        );
        auto_app.trust_mode_override = Some(umadev_agent::TrustMode::Auto);
        let (holder2, rx2) = install_host_request(&mut auto_app, 62, grok_plan_request("步骤一"));
        assert!(host_key(&holder2, &mut auto_app, KeyCode::Char('a')));
        let _ = rx2.blocking_recv().unwrap();
        assert_eq!(
            auto_app.effective_trust_mode(),
            umadev_agent::TrustMode::Auto,
            "an already-writable tier is never changed by a plan approval"
        );
    }

    #[test]
    fn grok_exit_plan_picker_has_three_explicit_states_and_feedback_escape_is_local() {
        for (token, key, expected) in [
            (31, KeyCode::Char('a'), "approved"),
            (32, KeyCode::Char('x'), "abandoned"),
        ] {
            let tmp = tempfile::TempDir::new().unwrap();
            let mut app = App::new(
                "grok-plan-picker",
                crate::config::UserConfig::default(),
                tmp.path().join("config.toml"),
                tmp.path().to_path_buf(),
            );
            let (holder, rx) =
                install_host_request(&mut app, token, grok_plan_request("第一步\nSecond step"));
            assert!(host_key(&holder, &mut app, key));
            let response = rx.blocking_recv().unwrap();
            assert!(matches!(
                (expected, response),
                (
                    "approved",
                    umadev_runtime::HostResponse::PlanOutcome {
                        outcome: umadev_runtime::HostPlanOutcome::Approved
                    }
                ) | (
                    "abandoned",
                    umadev_runtime::HostResponse::PlanOutcome {
                        outcome: umadev_runtime::HostPlanOutcome::Abandoned
                    }
                )
            ));
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "grok-plan-feedback",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let (holder, rx) = install_host_request(&mut app, 33, grok_plan_request("Revise me"));
        // Empty Enter has no implicit approval.
        assert!(host_key(&holder, &mut app, KeyCode::Enter));
        assert!(holder.lock().unwrap().is_some());
        assert!(host_key(&holder, &mut app, KeyCode::Char('r')));
        app.input = "Add rollback details".to_string();
        app.input_cursor = app.input.chars().count();
        // First Esc exits feedback editing; a second explicit revision can submit.
        assert!(host_key(&holder, &mut app, KeyCode::Esc));
        assert!(holder.lock().unwrap().is_some());
        assert!(host_key(&holder, &mut app, KeyCode::Char('r')));
        app.input = "Add rollback details".to_string();
        app.input_cursor = app.input.chars().count();
        assert!(host_key(&holder, &mut app, KeyCode::Enter));
        assert!(matches!(
            rx.blocking_recv().unwrap(),
            umadev_runtime::HostResponse::PlanOutcome {
                outcome: umadev_runtime::HostPlanOutcome::Cancelled {
                    feedback: Some(feedback)
                }
            } if feedback == "Add rollback details"
        ));
    }

    #[test]
    fn grok_exit_plan_panel_scrolls_complete_cjk_plan_and_marks_empty_plan() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "grok-plan-scroll",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let long_plan = "第一步：检查跨平台终端渲染和中文宽字符。第二步：验证所有交互状态。\n第三步：运行回归测试。";
        let (holder, _rx) = install_host_request(&mut app, 41, grok_plan_request(long_plan));
        let before = app
            .pending_host_input
            .as_ref()
            .unwrap()
            .panel_lines(umadev_i18n::Lang::ZhCn);
        assert!(host_key(&holder, &mut app, KeyCode::PageDown));
        let after = app
            .pending_host_input
            .as_ref()
            .unwrap()
            .panel_lines(umadev_i18n::Lang::ZhCn);
        assert_ne!(before[1], after[1]);

        let mut empty_app = App::new(
            "grok-plan-empty",
            crate::config::UserConfig::default(),
            tmp.path().join("empty-config.toml"),
            tmp.path().to_path_buf(),
        );
        let (_holder, _rx) = install_host_request(&mut empty_app, 42, grok_plan_request(""));
        assert!(empty_app
            .pending_host_input
            .as_ref()
            .unwrap()
            .panel_lines(umadev_i18n::Lang::ZhCn)
            .join("\n")
            .contains("未提供计划内容"));
    }

    fn folder_trust_request() -> HostRequest {
        HostRequest::FolderTrust {
            cwd: std::path::PathBuf::from("/repo/工作区/app"),
            workspace: std::path::PathBuf::from("/repo/工作区"),
            config_kinds: vec!["MCP 配置".to_string(), "项目规则".to_string()],
        }
    }

    #[test]
    fn folder_trust_defaults_to_keep_gated_and_renders_complete_cjk_scope() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "folder-trust-default",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let (holder, rx) = install_host_request(&mut app, 51, folder_trust_request());
        let panel = app
            .pending_host_input
            .as_ref()
            .unwrap()
            .panel_lines(umadev_i18n::Lang::ZhCn)
            .join("\n");
        assert!(panel.contains("/repo/工作区/app"));
        assert!(panel.contains("MCP 配置"));
        assert!(host_key(&holder, &mut app, KeyCode::Enter));
        assert_eq!(
            rx.blocking_recv().unwrap(),
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::KeepGated
            }
        );
    }

    #[test]
    fn folder_trust_requires_selection_and_second_enter_before_trust() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "folder-trust-double-confirm",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let (holder, mut rx) = install_host_request(&mut app, 52, folder_trust_request());
        assert!(host_key(&holder, &mut app, KeyCode::Down));
        assert!(host_key(&holder, &mut app, KeyCode::Enter));
        assert!(holder.lock().unwrap().is_some());
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(app
            .pending_host_input
            .as_ref()
            .unwrap()
            .panel_lines(umadev_i18n::Lang::En)
            .join("\n")
            .contains("TRUST ARMED"));
        assert!(host_key(&holder, &mut app, KeyCode::Enter));
        assert_eq!(
            rx.blocking_recv().unwrap(),
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::Trust
            }
        );
    }

    #[test]
    fn folder_trust_escape_disarms_then_keeps_gated() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut app = App::new(
            "folder-trust-escape",
            crate::config::UserConfig::default(),
            tmp.path().join("config.toml"),
            tmp.path().to_path_buf(),
        );
        let (holder, mut rx) = install_host_request(&mut app, 53, folder_trust_request());
        assert!(host_key(&holder, &mut app, KeyCode::Char('t')));
        assert!(host_key(&holder, &mut app, KeyCode::Esc));
        assert!(holder.lock().unwrap().is_some());
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(host_key(&holder, &mut app, KeyCode::Esc));
        assert_eq!(
            rx.blocking_recv().unwrap(),
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::KeepGated
            }
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuestionFocus {
    Options,
    Notes,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct QuestionProgress {
    cursor: usize,
    selected: Vec<usize>,
    notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrokQuestionState {
    questions: Vec<HostQuestion>,
    progress: Vec<QuestionProgress>,
    current: usize,
    focus: QuestionFocus,
    plan_mode: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GrokPlanState {
    cursor: Option<usize>,
    editing_feedback: bool,
    plan_rows: Vec<String>,
    plan_scroll: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FolderTrustState {
    cwd: String,
    workspace: String,
    config_kinds: Vec<String>,
    /// `0` remains gated (the safe default), `1` requests trust.
    cursor: usize,
    /// Trust requires a second, independent Enter. Merely focusing or choosing
    /// the trust row never grants authority.
    trust_armed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostInputKind {
    Generic,
    GrokQuestion(GrokQuestionState),
    GrokPlan(GrokPlanState),
    FolderTrust(FolderTrustState),
}

/// Renderer-facing state for one same-RPC interaction. Vendor-specific state
/// lives here, outside `app.rs`, so repeated holder synchronisation cannot reset
/// the focused option, selected answers, previews, or notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHostInputView {
    token: u64,
    summary: String,
    secret: bool,
    kind: HostInputKind,
}

/// Result of handling a key that belongs to a contract-specific picker.
pub(crate) enum HostInputKeyOutcome {
    /// Let the ordinary UTF-8 editor handle this key (notes/feedback entry).
    Passthrough,
    /// The picker consumed the key but has not completed yet.
    Consumed,
    /// Keep the picker open and surface a visible validation message.
    Invalid(String),
    /// Complete the exact in-flight request with a typed protocol outcome.
    Respond { token: u64, response: HostResponse },
}

fn response_contract(request: &HostRequest) -> Option<&str> {
    match request {
        HostRequest::UserInput { metadata, .. }
        | HostRequest::PlanConfirmation { metadata, .. } => metadata
            .get("responseContract")
            .and_then(serde_json::Value::as_str),
        _ => None,
    }
}

fn request_summary(request: &HostRequest) -> String {
    match request {
        HostRequest::UserInput { questions, .. } => questions.first().map_or_else(
            || "base requested user input".to_string(),
            |question| {
                question.header.as_ref().map_or_else(
                    || question.prompt.clone(),
                    |header| format!("{header}: {}", question.prompt),
                )
            },
        ),
        HostRequest::McpElicitation {
            server_name,
            message,
            ..
        } => server_name
            .as_ref()
            .map_or_else(|| message.clone(), |server| format!("{server}: {message}")),
        HostRequest::PlanConfirmation { message, .. } => message
            .clone()
            .unwrap_or_else(|| "Review the proposed plan".to_string()),
        HostRequest::FolderTrust { workspace, .. } => {
            format!("Grok Build folder trust: {}", workspace.display())
        }
        _ => "base requested a response".to_string(),
    }
}

fn request_is_secret(request: &HostRequest) -> bool {
    matches!(
        request,
        HostRequest::UserInput { questions, .. }
            if questions.iter().any(|question| {
                matches!(question.kind, HostQuestionKind::Secret)
            })
    )
}

impl PendingHostInputView {
    fn new(descriptor: HostInputDescriptor) -> Self {
        let kind = match &descriptor.request {
            HostRequest::UserInput {
                questions,
                metadata,
            } if response_contract(&descriptor.request) == Some(GROK_QUESTION_CONTRACT) => {
                let progress = vec![QuestionProgress::default(); questions.len()];
                HostInputKind::GrokQuestion(GrokQuestionState {
                    questions: questions.clone(),
                    progress,
                    current: 0,
                    focus: QuestionFocus::Options,
                    plan_mode: metadata.get("mode").and_then(serde_json::Value::as_str)
                        == Some("plan"),
                })
            }
            HostRequest::PlanConfirmation { plan, .. }
                if response_contract(&descriptor.request) == Some(GROK_PLAN_CONTRACT) =>
            {
                HostInputKind::GrokPlan(GrokPlanState {
                    plan_rows: wrap_plan_rows(plan),
                    ..GrokPlanState::default()
                })
            }
            HostRequest::FolderTrust {
                cwd,
                workspace,
                config_kinds,
            } => HostInputKind::FolderTrust(FolderTrustState {
                cwd: cwd.to_string_lossy().into_owned(),
                workspace: workspace.to_string_lossy().into_owned(),
                config_kinds: config_kinds.clone(),
                cursor: 0,
                trust_armed: false,
            }),
            _ => HostInputKind::Generic,
        };
        Self {
            token: descriptor.token,
            summary: request_summary(&descriptor.request),
            secret: request_is_secret(&descriptor.request),
            kind,
        }
    }

    pub(crate) fn token(&self) -> u64 {
        self.token
    }

    pub(crate) fn is_secret(&self) -> bool {
        self.secret
    }

    pub(crate) fn panel_height(&self) -> u16 {
        match self.kind {
            HostInputKind::Generic => 1,
            HostInputKind::GrokQuestion(_) => 6,
            HostInputKind::GrokPlan(_) | HostInputKind::FolderTrust(_) => 7,
        }
    }

    /// Compact, already-bounded rows for the sticky interaction panel. The
    /// renderer applies terminal-width clipping; previews are limited to two
    /// lines here so an option cannot crowd the transcript off-screen.
    pub(crate) fn panel_lines(&self, lang: umadev_i18n::Lang) -> Vec<String> {
        match &self.kind {
            HostInputKind::Generic => vec![self.summary.clone()],
            HostInputKind::GrokPlan(state) => {
                let actions = [
                    umadev_i18n::t(lang, "host.grok.plan.approve"),
                    umadev_i18n::t(lang, "host.grok.plan.revise"),
                    umadev_i18n::t(lang, "host.grok.plan.abandon"),
                ];
                let mut lines = vec![format!(
                    "{} · {}",
                    umadev_i18n::t(lang, "host.grok.plan.title"),
                    self.summary
                )];
                let plan_rows = if state.plan_rows.is_empty() {
                    vec![
                        format!("  {}", umadev_i18n::t(lang, "host.grok.plan.empty")),
                        String::new(),
                    ]
                } else {
                    (0..2)
                        .map(|offset| {
                            state
                                .plan_rows
                                .get(state.plan_scroll + offset)
                                .map_or_else(String::new, |line| format!("  {line}"))
                        })
                        .collect::<Vec<_>>()
                };
                // The first plan row plus all three decisions fit in the
                // 40x10 minimum layout. Extra plan context and hints use the
                // remaining rows on ordinary terminals.
                lines.push(plan_rows[0].clone());
                lines.extend(actions.into_iter().enumerate().map(|(index, action)| {
                    format!(
                        "{} {action}",
                        if Some(index) == state.cursor {
                            ">"
                        } else {
                            " "
                        }
                    )
                }));
                lines.push(plan_rows[1].clone());
                lines.push(if state.editing_feedback {
                    umadev_i18n::t(lang, "host.grok.plan.feedback_hint").to_string()
                } else {
                    umadev_i18n::t(lang, "host.grok.plan.hint").to_string()
                });
                lines
            }
            HostInputKind::GrokQuestion(state) => {
                let Some(question) = state.questions.get(state.current) else {
                    return vec![umadev_i18n::t(lang, "host.grok.question.empty").to_string()];
                };
                let progress = &state.progress[state.current];
                let current = (state.current + 1).to_string();
                let total = state.questions.len().to_string();
                let mut lines = vec![umadev_i18n::tf(
                    lang,
                    "host.grok.question.title",
                    &[&current, &total, &question.prompt],
                )];
                if let Some(option) = question.options.get(progress.cursor) {
                    let selected = progress.selected.contains(&progress.cursor);
                    lines.push(format!(
                        "> [{}] {}{}",
                        if selected { 'x' } else { ' ' },
                        option.label,
                        option
                            .description
                            .as_deref()
                            .map(|description| format!(" — {description}"))
                            .unwrap_or_default()
                    ));
                    if let Some(preview) = &option.preview {
                        lines.extend(preview.lines().take(2).map(|line| {
                            umadev_i18n::tf(lang, "host.grok.question.preview", &[line])
                        }));
                    }
                }
                lines.push(match state.focus {
                    QuestionFocus::Options => {
                        umadev_i18n::t(lang, "host.grok.question.options_hint").to_string()
                    }
                    QuestionFocus::Notes if progress.notes.is_empty() => {
                        umadev_i18n::t(lang, "host.grok.question.notes_hint").to_string()
                    }
                    QuestionFocus::Notes => {
                        umadev_i18n::t(lang, "host.grok.question.notes_saved_hint").to_string()
                    }
                });
                if state.plan_mode {
                    lines.push(umadev_i18n::t(lang, "host.grok.question.plan_hint").to_string());
                } else {
                    lines.push(umadev_i18n::t(lang, "host.grok.question.cancel_hint").to_string());
                }
                lines.truncate(7);
                lines
            }
            HostInputKind::FolderTrust(state) => {
                let options = [
                    umadev_i18n::t(lang, "host.grok.folder_trust.keep_gated"),
                    umadev_i18n::t(lang, "host.grok.folder_trust.trust"),
                ];
                let mut lines =
                    vec![umadev_i18n::t(lang, "host.grok.folder_trust.title").to_string()];
                lines.push(umadev_i18n::tf(
                    lang,
                    "host.grok.folder_trust.workspace",
                    &[&state.workspace],
                ));
                lines.push(umadev_i18n::tf(
                    lang,
                    "host.grok.folder_trust.cwd",
                    &[&state.cwd],
                ));
                let kinds = state
                    .config_kinds
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let extra = state.config_kinds.len().saturating_sub(3);
                let kinds = if extra == 0 {
                    kinds
                } else {
                    format!("{kinds} (+{extra})")
                };
                lines.push(umadev_i18n::tf(
                    lang,
                    "host.grok.folder_trust.config",
                    &[&kinds],
                ));
                lines.extend(options.into_iter().enumerate().map(|(index, option)| {
                    format!("{} {option}", if index == state.cursor { ">" } else { " " })
                }));
                lines.push(
                    umadev_i18n::t(
                        lang,
                        if state.trust_armed {
                            "host.grok.folder_trust.confirm_hint"
                        } else {
                            "host.grok.folder_trust.hint"
                        },
                    )
                    .to_string(),
                );
                lines
            }
        }
    }

    fn specialized(&self) -> bool {
        !matches!(self.kind, HostInputKind::Generic)
    }
}

/// Wrap the complete plan into conservative 24-cell rows. The TUI's supported
/// minimum width leaves at least this much room after gutters, so PgUp/PgDn can
/// eventually reveal every CJK/emoji/code cell even on a narrow terminal.
fn wrap_plan_rows(plan: &str) -> Vec<String> {
    const VIEW_CELLS: usize = 24;
    let mut rows = Vec::new();
    for source in plan.lines() {
        if source.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut width = 0usize;
        for ch in source.chars() {
            let ch_width = ch.width_cjk().unwrap_or(0);
            if !row.is_empty() && width.saturating_add(ch_width) > VIEW_CELLS {
                rows.push(std::mem::take(&mut row));
                width = 0;
            }
            row.push(ch);
            width = width.saturating_add(ch_width);
        }
        rows.push(row);
    }
    rows
}

fn set_composer_text(app: &mut App, text: String) {
    app.input = text;
    app.input_cursor = app.input.chars().count();
    app.input_selection = None;
    app.input_selection_dragging = false;
}

fn save_question_notes(app: &mut App, state: &mut GrokQuestionState) {
    if state.focus == QuestionFocus::Notes {
        state.progress[state.current].notes = app.input.clone();
    }
}

fn switch_question(app: &mut App, state: &mut GrokQuestionState, next: usize) {
    save_question_notes(app, state);
    state.current = next.min(state.questions.len().saturating_sub(1));
    if state.focus == QuestionFocus::Notes {
        set_composer_text(app, state.progress[state.current].notes.clone());
    }
}

fn select_focused_option(state: &mut GrokQuestionState) -> Result<(), String> {
    let question = state
        .questions
        .get(state.current)
        .ok_or_else(|| "the questionnaire contains no questions".to_string())?;
    let progress = &mut state.progress[state.current];
    if question.options.get(progress.cursor).is_none() {
        return Err("the current question contains no selectable options".to_string());
    }
    if matches!(question.kind, HostQuestionKind::MultiChoice) {
        if let Some(index) = progress
            .selected
            .iter()
            .position(|selected| *selected == progress.cursor)
        {
            progress.selected.remove(index);
        } else {
            progress.selected.push(progress.cursor);
            progress.selected.sort_unstable();
        }
    } else {
        progress.selected.clear();
        progress.selected.push(progress.cursor);
    }
    Ok(())
}

fn question_is_answered(question: &HostQuestion, progress: &QuestionProgress) -> bool {
    !progress.selected.is_empty() || !progress.notes.trim().is_empty() || !question.required
}

fn question_answers(state: &GrokQuestionState, partial: bool) -> Vec<HostAnswer> {
    state
        .questions
        .iter()
        .zip(&state.progress)
        .filter_map(|(question, progress)| {
            let mut values = progress
                .selected
                .iter()
                .filter_map(|selected| question.options.get(*selected))
                .map(|option| option.value.clone())
                .collect::<Vec<_>>();
            if partial && values.is_empty() && !progress.notes.trim().is_empty() {
                // Grok's partial contract intentionally drops note text, but a
                // notes-only answer remains semantically present as Other.
                values.push("Other".to_string());
            }
            (!partial || !values.is_empty()).then(|| HostAnswer {
                question_id: question.id.clone(),
                values,
            })
        })
        .collect()
}

fn question_annotations(state: &GrokQuestionState) -> Vec<HostQuestionAnnotation> {
    state
        .questions
        .iter()
        .zip(&state.progress)
        .filter_map(|(question, progress)| {
            let preview = if matches!(question.kind, HostQuestionKind::SingleChoice)
                && progress.selected.len() == 1
            {
                question
                    .options
                    .get(progress.selected[0])
                    .and_then(|option| option.preview.clone())
            } else {
                None
            };
            let notes = (!progress.notes.trim().is_empty()).then(|| progress.notes.clone());
            (preview.is_some() || notes.is_some()).then(|| HostQuestionAnnotation {
                question_id: question.id.clone(),
                preview,
                notes,
            })
        })
        .collect()
}

fn accepted_question_response(state: &GrokQuestionState) -> Result<HostResponse, String> {
    if let Some((question, _)) = state
        .questions
        .iter()
        .zip(&state.progress)
        .find(|(question, progress)| !question_is_answered(question, progress))
    {
        return Err(format!(
            "`{}` requires a selection or note",
            question.prompt
        ));
    }
    Ok(HostResponse::UserInputOutcome {
        outcome: HostUserInputOutcome::Accepted {
            answers: question_answers(state, false),
            annotations: question_annotations(state),
        },
    })
}

impl App {
    /// Contract-specific picker routing. `None` means the pending request is a
    /// generic free-form interaction and should keep using the legacy composer
    /// parser. Modified chords deliberately pass through to global shortcuts.
    pub(crate) fn resolve_specialized_host_input_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> Option<HostInputKeyOutcome> {
        if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) {
            return None;
        }
        let mut view = self.pending_host_input.take()?;
        if !view.specialized() {
            self.pending_host_input = Some(view);
            return None;
        }

        let outcome = if code == KeyCode::Esc && modifiers.is_empty() {
            match &mut view.kind {
                HostInputKind::GrokQuestion(state) if state.focus == QuestionFocus::Notes => {
                    save_question_notes(self, state);
                    set_composer_text(self, String::new());
                    state.focus = QuestionFocus::Options;
                    HostInputKeyOutcome::Consumed
                }
                HostInputKind::GrokPlan(state) if state.editing_feedback => {
                    set_composer_text(self, String::new());
                    state.editing_feedback = false;
                    HostInputKeyOutcome::Consumed
                }
                HostInputKind::FolderTrust(state) if state.trust_armed => {
                    state.trust_armed = false;
                    HostInputKeyOutcome::Consumed
                }
                kind => {
                    let response = match kind {
                        HostInputKind::GrokQuestion(_) => HostResponse::UserInputOutcome {
                            outcome: HostUserInputOutcome::Cancelled,
                        },
                        HostInputKind::GrokPlan(_) => HostResponse::PlanOutcome {
                            outcome: HostPlanOutcome::Cancelled { feedback: None },
                        },
                        HostInputKind::FolderTrust(_) => HostResponse::FolderTrust {
                            decision: HostFolderTrustDecision::KeepGated,
                        },
                        HostInputKind::Generic => {
                            unreachable!("generic interaction returned above")
                        }
                    };
                    HostInputKeyOutcome::Respond {
                        token: view.token,
                        response,
                    }
                }
            }
        } else {
            match &mut view.kind {
                HostInputKind::GrokQuestion(state) => {
                    self.handle_grok_question_key(view.token, state, code, modifiers)
                }
                HostInputKind::GrokPlan(state) => {
                    self.handle_grok_plan_key(view.token, state, code, modifiers)
                }
                HostInputKind::FolderTrust(state) => {
                    Self::handle_folder_trust_key(view.token, state, code)
                }
                HostInputKind::Generic => unreachable!("generic interaction returned above"),
            }
        };
        self.pending_host_input = Some(view);
        Some(outcome)
    }

    fn handle_grok_question_key(
        &mut self,
        token: u64,
        state: &mut GrokQuestionState,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> HostInputKeyOutcome {
        if state.questions.is_empty() {
            return HostInputKeyOutcome::Invalid(
                "the base supplied an empty questionnaire; press Esc to cancel".to_string(),
            );
        }
        if code == KeyCode::Enter && modifiers.contains(KeyModifiers::SHIFT) {
            return HostInputKeyOutcome::Passthrough;
        }
        if state.focus == QuestionFocus::Notes {
            match code {
                KeyCode::Tab | KeyCode::BackTab => {
                    save_question_notes(self, state);
                    set_composer_text(self, String::new());
                    state.focus = QuestionFocus::Options;
                    return HostInputKeyOutcome::Consumed;
                }
                KeyCode::Enter if modifiers.is_empty() => {
                    save_question_notes(self, state);
                    if state.current + 1 < state.questions.len() {
                        state.focus = QuestionFocus::Options;
                        set_composer_text(self, String::new());
                        state.current += 1;
                        return HostInputKeyOutcome::Consumed;
                    }
                    return match accepted_question_response(state) {
                        Ok(response) => HostInputKeyOutcome::Respond { token, response },
                        Err(error) => HostInputKeyOutcome::Invalid(error),
                    };
                }
                _ => return HostInputKeyOutcome::Passthrough,
            }
        }

        match code {
            KeyCode::Up => {
                let progress = &mut state.progress[state.current];
                progress.cursor = progress.cursor.saturating_sub(1);
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Down => {
                let max = state.questions[state.current]
                    .options
                    .len()
                    .saturating_sub(1);
                let progress = &mut state.progress[state.current];
                progress.cursor = progress.cursor.saturating_add(1).min(max);
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Left | KeyCode::Char('[') => {
                let next = state.current.saturating_sub(1);
                switch_question(self, state, next);
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Right | KeyCode::Char(']') => {
                let next = state
                    .current
                    .saturating_add(1)
                    .min(state.questions.len().saturating_sub(1));
                switch_question(self, state, next);
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Char(' ') => match select_focused_option(state) {
                Ok(()) => HostInputKeyOutcome::Consumed,
                Err(error) => HostInputKeyOutcome::Invalid(error),
            },
            KeyCode::Char(digit @ '1'..='9') => {
                let index = usize::try_from(digit.to_digit(10).unwrap_or_default())
                    .unwrap_or_default()
                    .saturating_sub(1);
                if index >= state.questions[state.current].options.len() {
                    return HostInputKeyOutcome::Invalid(format!(
                        "option {} is not available for this question",
                        index + 1
                    ));
                }
                state.progress[state.current].cursor = index;
                match select_focused_option(state) {
                    Ok(()) => HostInputKeyOutcome::Consumed,
                    Err(error) => HostInputKeyOutcome::Invalid(error),
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.focus = QuestionFocus::Notes;
                set_composer_text(self, state.progress[state.current].notes.clone());
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Char('c') if state.plan_mode && self.input.is_empty() => {
                HostInputKeyOutcome::Respond {
                    token,
                    response: HostResponse::UserInputOutcome {
                        outcome: HostUserInputOutcome::ChatAboutThis {
                            partial_answers: question_answers(state, true),
                        },
                    },
                }
            }
            KeyCode::Char('s') if state.plan_mode && self.input.is_empty() => {
                HostInputKeyOutcome::Respond {
                    token,
                    response: HostResponse::UserInputOutcome {
                        outcome: HostUserInputOutcome::SkipInterview {
                            partial_answers: question_answers(state, true),
                        },
                    },
                }
            }
            KeyCode::Enter if modifiers.is_empty() => {
                // FOCUS IS SELECTION for a single-choice question: the cursor
                // already points at an option (`> [ ] A. …`), so Enter confirms
                // that option directly. Demanding a separate Space first
                // rejected the user's most natural reply — Enter on the focused
                // row — with "requires a selection or note", four times in the
                // reported session, while the base itself had told them to just
                // reply with the option. Multi-choice keeps the explicit Space
                // toggles (Enter alone stays ambiguous there).
                {
                    let question = &state.questions[state.current];
                    let progress = &mut state.progress[state.current];
                    if matches!(question.kind, HostQuestionKind::SingleChoice)
                        && progress.selected.is_empty()
                        && question.options.get(progress.cursor).is_some()
                    {
                        progress.selected = vec![progress.cursor];
                    }
                }
                let progress = &state.progress[state.current];
                if !question_is_answered(&state.questions[state.current], progress) {
                    return HostInputKeyOutcome::Invalid(format!(
                        "`{}` requires a selection or note",
                        state.questions[state.current].prompt
                    ));
                }
                if state.current + 1 < state.questions.len() {
                    state.current += 1;
                    HostInputKeyOutcome::Consumed
                } else {
                    match accepted_question_response(state) {
                        Ok(response) => HostInputKeyOutcome::Respond { token, response },
                        Err(error) => HostInputKeyOutcome::Invalid(error),
                    }
                }
            }
            _ => HostInputKeyOutcome::Consumed,
        }
    }

    fn handle_folder_trust_key(
        token: u64,
        state: &mut FolderTrustState,
        code: KeyCode,
    ) -> HostInputKeyOutcome {
        match code {
            KeyCode::Up | KeyCode::Left => {
                state.cursor = 0;
                state.trust_armed = false;
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Down | KeyCode::Right => {
                state.cursor = 1;
                state.trust_armed = false;
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Char('r' | 'R') => HostInputKeyOutcome::Respond {
                token,
                response: HostResponse::FolderTrust {
                    decision: HostFolderTrustDecision::KeepGated,
                },
            },
            KeyCode::Char('t' | 'T') => {
                state.cursor = 1;
                state.trust_armed = true;
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Enter if state.cursor == 0 => HostInputKeyOutcome::Respond {
                token,
                response: HostResponse::FolderTrust {
                    decision: HostFolderTrustDecision::KeepGated,
                },
            },
            KeyCode::Enter if state.trust_armed => HostInputKeyOutcome::Respond {
                token,
                response: HostResponse::FolderTrust {
                    decision: HostFolderTrustDecision::Trust,
                },
            },
            KeyCode::Enter => {
                state.trust_armed = true;
                HostInputKeyOutcome::Consumed
            }
            _ => HostInputKeyOutcome::Consumed,
        }
    }

    fn handle_grok_plan_key(
        &mut self,
        token: u64,
        state: &mut GrokPlanState,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> HostInputKeyOutcome {
        if code == KeyCode::Enter && modifiers.contains(KeyModifiers::SHIFT) {
            return if state.editing_feedback {
                HostInputKeyOutcome::Passthrough
            } else {
                HostInputKeyOutcome::Consumed
            };
        }
        if state.editing_feedback {
            return match code {
                KeyCode::Enter if modifiers.is_empty() => {
                    let feedback = self.input.trim();
                    if feedback.is_empty() {
                        HostInputKeyOutcome::Invalid(
                            "enter revision feedback before submitting".to_string(),
                        )
                    } else {
                        HostInputKeyOutcome::Respond {
                            token,
                            response: HostResponse::PlanOutcome {
                                outcome: HostPlanOutcome::Cancelled {
                                    feedback: Some(feedback.to_string()),
                                },
                            },
                        }
                    }
                }
                _ => HostInputKeyOutcome::Passthrough,
            };
        }
        match code {
            KeyCode::PageUp => {
                state.plan_scroll = state.plan_scroll.saturating_sub(2);
                HostInputKeyOutcome::Consumed
            }
            KeyCode::PageDown => {
                state.plan_scroll = state
                    .plan_scroll
                    .saturating_add(2)
                    .min(state.plan_rows.len().saturating_sub(2));
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Up => {
                state.cursor = Some(state.cursor.unwrap_or(0).saturating_sub(1));
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Down => {
                state.cursor = Some(
                    state
                        .cursor
                        .map_or(0, |cursor| cursor.saturating_add(1).min(2)),
                );
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Char('a') => HostInputKeyOutcome::Respond {
                token,
                response: HostResponse::PlanOutcome {
                    outcome: HostPlanOutcome::Approved,
                },
            },
            KeyCode::Char('r') => {
                state.cursor = Some(1);
                state.editing_feedback = true;
                set_composer_text(self, String::new());
                HostInputKeyOutcome::Consumed
            }
            KeyCode::Char('x') => HostInputKeyOutcome::Respond {
                token,
                response: HostResponse::PlanOutcome {
                    outcome: HostPlanOutcome::Abandoned,
                },
            },
            KeyCode::Enter if modifiers.is_empty() => match state.cursor {
                Some(0) => HostInputKeyOutcome::Respond {
                    token,
                    response: HostResponse::PlanOutcome {
                        outcome: HostPlanOutcome::Approved,
                    },
                },
                Some(1) => {
                    state.editing_feedback = true;
                    set_composer_text(self, String::new());
                    HostInputKeyOutcome::Consumed
                }
                Some(2) => HostInputKeyOutcome::Respond {
                    token,
                    response: HostResponse::PlanOutcome {
                        outcome: HostPlanOutcome::Abandoned,
                    },
                },
                _ => HostInputKeyOutcome::Invalid(
                    "select a plan outcome before confirming".to_string(),
                ),
            },
            _ => HostInputKeyOutcome::Consumed,
        }
    }
}

/// Composer state parked while an in-flight base RPC temporarily owns the input
/// box. This prevents a half-written future chat message from being submitted as
/// the answer to a newly-arrived host question.
#[derive(Debug, Clone)]
pub(super) struct HostInputDraft {
    input: String,
    input_cursor: usize,
    attachments: Vec<std::path::PathBuf>,
    text_stash: Vec<String>,
}

impl App {
    fn clear_sensitive_edit_recovery(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.kill_ring.clear();
        self.last_snapshot_at = None;
        self.last_kill = None;
        self.yank_span = None;
        self.yank_ring_idx = 0;
        self.input_selection = None;
        self.input_selection_dragging = false;
    }

    pub(crate) fn discard_host_response_input(&mut self, secret: bool) {
        self.clear_input();
        if secret {
            self.clear_sensitive_edit_recovery();
        }
    }

    /// Mirror one in-flight typed host prompt into the renderer. Returns whether
    /// the visible state changed. The appearance edge uses the same completion
    /// bell as an approval pause because both mean a long-running turn now needs
    /// the user's attention.
    pub(crate) fn set_pending_host_input(&mut self, item: Option<HostInputDescriptor>) -> bool {
        let next_token = item.as_ref().map(|item| item.token);
        if self
            .pending_host_input
            .as_ref()
            .map(PendingHostInputView::token)
            == next_token
        {
            return false;
        }
        let was_secret = self
            .pending_host_input
            .as_ref()
            .is_some_and(PendingHostInputView::is_secret);
        let entering_secret = item
            .as_ref()
            .is_some_and(|item| request_is_secret(&item.request))
            && !was_secret;
        if item.is_some() && self.pending_host_input.is_none() {
            self.arm_completion_bell(self.thinking_started);
            if !self.input.is_empty() || !self.attachments.is_empty() || !self.text_stash.is_empty()
            {
                self.host_input_draft = Some(HostInputDraft {
                    input: std::mem::take(&mut self.input),
                    input_cursor: self.input_cursor,
                    attachments: std::mem::take(&mut self.attachments),
                    text_stash: std::mem::take(&mut self.text_stash),
                });
                self.input_cursor = 0;
                self.input_history_idx = None;
                self.input_history_draft = None;
                self.input_selection = None;
                self.input_selection_dragging = false;
            }
        } else if item.is_none() && self.pending_host_input.is_some() {
            // A partial/secret answer belongs only to the now-settled RPC. Clear
            // it before restoring the unrelated chat draft that was composing
            // when the base question arrived.
            self.discard_host_response_input(was_secret);
            if let Some(draft) = self.host_input_draft.take() {
                self.input = draft.input;
                self.input_cursor = draft.input_cursor.min(self.input_len());
                self.attachments = draft.attachments;
                self.text_stash = draft.text_stash;
            }
        } else if item.is_some() {
            // A newer request replaced an older one before the next frame. The
            // parked ordinary chat draft still belongs outside both RPCs; only
            // clear the superseded response buffer before initialising the new
            // token's state.
            self.discard_host_response_input(was_secret);
        }
        if entering_secret {
            self.clear_sensitive_edit_recovery();
        }
        self.pending_host_input = item.map(PendingHostInputView::new);
        true
    }

    /// Text painted in the composer. A secret host question keeps the actual
    /// UTF-8 buffer for submission but exposes only one bullet per non-newline
    /// character to the terminal renderer.
    pub(crate) fn rendered_input(&self) -> Cow<'_, str> {
        if self
            .pending_host_input
            .as_ref()
            .is_some_and(PendingHostInputView::is_secret)
        {
            Cow::Owned(
                self.input
                    .chars()
                    .map(|ch| if ch == '\n' { '\n' } else { '•' })
                    .collect(),
            )
        } else {
            Cow::Borrowed(&self.input)
        }
    }

    /// Consume the current composer as an answer to an in-flight host request.
    /// Ordinary answers remain visible and become bounded conversation memory;
    /// secrets are cleared without entering history, transcript persistence, or
    /// chat rendering. Attachment/paste chips are expanded before the buffer is
    /// cleared so structured JSON pasted through a collapsed chip is preserved.
    pub(crate) fn accept_host_response_input(&mut self, secret: bool) -> String {
        let raw = self.prepared_host_response_input();
        self.discard_host_response_input(secret);
        self.transcript_scroll_to_bottom();
        if !secret && !raw.is_empty() {
            self.remember_submission(&raw);
            self.push(ChatRole::You, raw.clone());
            self.record_user_turn(&raw);
            self.persist_chat();
        }
        self.refresh_status();
        raw
    }

    /// Expand composer chips without consuming the buffer. The event loop uses
    /// this to validate a structured answer before clearing anything, so a JSON
    /// typo leaves the user's draft intact for correction.
    pub(crate) fn prepared_host_response_input(&self) -> String {
        self.expand_attachments(self.input.trim())
    }
}
