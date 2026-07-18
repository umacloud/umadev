//! Shared ACP v1 subprocess driver.
//!
//! Grok Build and Kimi Code expose the Agent Client Protocol lifecycle over
//! newline-delimited JSON-RPC. This module owns the hardened shared transport;
//! each vendor's executable, identity, authentication, permissions and private
//! extensions remain explicitly isolated in [`AcpVendor`] and source contracts.
//!
//! The implementation deliberately does not use an SDK transport. UmaDev's
//! existing subprocess boundary remains authoritative for Windows npm shims,
//! bounded queues, cancellation, stderr diagnostics, and child cleanup. The
//! official `agent-client-protocol-schema` crate is used for the stable v1
//! request types; tolerant `serde_json::Value` decoding is retained at the
//! vendor-extension edge so a private notification can never crash the host.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use agent_client_protocol_schema::v1::{
    BlobResourceContents, ClientCapabilities, CloseSessionRequest, ContentBlock, EmbeddedResource,
    EmbeddedResourceResource, FileSystemCapabilities, ImageContent, Implementation,
    InitializeRequest, LoadSessionRequest, NewSessionRequest, PromptRequest, ResumeSessionRequest,
    TextResourceContents,
};
use agent_client_protocol_schema::ProtocolVersion;
use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

use umadev_runtime::{
    ApprovalDecision, BackgroundProcessControlCapability, BackgroundProcessInfo,
    BackgroundProcessKind, BackgroundProcessSignal, BackgroundProcessSnapshot,
    BackgroundProcessStopOutcome, BackgroundTaskSignal, BasePermissionProfile, BaseSession,
    BrainCapabilities, CompletionRequest, CompletionResponse, DeliveryReceiptStage, DeliveryReport,
    HostAnswer, HostApprovalOption, HostApprovalOptionKind, HostElicitationAction,
    HostFolderTrustDecision, HostPermission, HostPlanOutcome, HostQuestion, HostQuestionAnnotation,
    HostQuestionKind, HostQuestionOption, HostRequest, HostResponse, HostUserInputOutcome,
    InputDelivery, PromptQueueCapability, PromptQueueMutation, PromptQueuePlacement,
    PromptQueueSnapshot, ResumeCapability, Runtime, RuntimeError, RuntimeKind, SessionCapabilities,
    SessionCapability, SessionCommandInfo, SessionError, SessionEvent, SessionMode,
    SessionModelInfo, SessionPlanEntry, SessionPlanEntryPriority, SessionPlanEntryStatus,
    SessionReasoningEffort, SessionReasoningEffortOption, SessionStateUpdate, SteerSemantics,
    StreamEvent, SubagentVisibility, TurnInput, TurnInputBlockKind, TurnStatus, Usage,
};

use crate::folder_trust::{
    folder_trust_client_capabilities_meta, FolderTrustClientSurface, FolderTrustRequest,
    FolderTrustScope, FolderTrustUserDecision, GROK_FOLDER_TRUST_REQUEST_METHOD,
    GROK_FOLDER_TRUST_TIMEOUT,
};
use crate::grok_auth_flow::{
    GrokAuthAction, GrokAuthAttempt, GrokAuthAttemptError, GrokAuthRpc, GrokAuthStopReason,
    GrokInteractiveAuthOptions, GrokSensitiveAuthRpc,
};
use crate::grok_background_control::{
    kill_params as grok_task_kill_params, list_params as grok_task_list_params,
    parse_kill_response as parse_grok_task_kill_response,
    parse_list_response as parse_grok_task_list_response, GROK_TASK_KILL_METHOD,
    GROK_TASK_LIST_METHOD,
};
use crate::grok_contract::{source_profile_from_initialize, GrokSourceCapability};
use crate::grok_prompt_queue::{
    grok_prompt_meta as grok_queue_prompt_meta, GrokPromptQueue, GrokQueueMutation,
    GrokQueueSnapshot, QUEUE_CHANGED_METHOD,
};
use crate::grok_routes::{ConvergedTerminal, LifecycleEffect, SessionRouteState};
use crate::kimi_contract::source_profile_from_initialize as kimi_source_profile_from_initialize;
use crate::session_bootstrap::{
    AuthCommand, AuthControl, AuthOffer, GrokAuthCatalog, SessionOpenError, SessionOpenEvent,
    SessionOpenEventSender, SessionOpenId, SessionOpenPolicy,
};
use crate::stderr_tail::{StderrDrain, StderrTail};
use crate::{
    default_workspace, govern_root_env, home_dir, isolate_process_tree, kill_isolated_process_tree,
    merge_prompt, reap_isolated_process_tree, resolve_program, run_subprocess, spawn_parts,
    spawn_retrying_etxtbsy, AuthState, HostDriver, ProbeResult, PromptChannel, SubprocessCall,
    TerminalTextSanitizer, END_REAP_BUDGET,
};

const EVENT_CHANNEL_CAP: usize = 256;
const MAX_PENDING_REQUESTS: usize = 128;
const MAX_PENDING_APPROVALS: usize = 64;
const MAX_SEEN_GROK_EVENT_IDS: usize = 4_096;
const MAX_REPLAY_SUBAGENTS: usize = 1_024;
const MAX_BACKGROUND_PROCESSES: usize = 1_024;
const MAX_COMPLETED_BACKGROUND_PROCESSES: usize = 4_096;
const MAX_BACKGROUND_ID_CHARS: usize = 512;
const MAX_BACKGROUND_DESCRIPTION_CHARS: usize = 4_096;
const MAX_BACKGROUND_COMMAND_CHARS: usize = 64 * 1024;
const MAX_BACKGROUND_PATH_CHARS: usize = 16 * 1024;
const MAX_BACKGROUND_SIGNAL_CHARS: usize = 256;
const MAX_PERMISSION_FOLLOWUP_CHARS: usize = 4_096;
const MAX_GROK_QUESTIONS: usize = 64;
const MAX_GROK_OPTIONS_PER_QUESTION: usize = 128;
const MAX_GROK_ID_CHARS: usize = 512;
const MAX_GROK_QUESTION_CHARS: usize = 4_096;
const MAX_GROK_OPTION_LABEL_CHARS: usize = 512;
const MAX_GROK_OPTION_DESCRIPTION_CHARS: usize = 4_096;
const MAX_GROK_PREVIEW_CHARS: usize = 64 * 1024;
const MAX_GROK_NOTES_CHARS: usize = 16 * 1024;
const MAX_GROK_PLAN_CHARS: usize = 256 * 1024;
const MAX_KIMI_PLAN_REVIEW_CHARS: usize = 256 * 1024;
const MAX_TOOL_CALL_ID_CHARS: usize = 512;
// Kimi's official adapter carries the final tool output in the terminal
// `tool_call_update` rather than streaming stdout progress. Keep enough of that
// result for `/logs` and expanded tool cards; the TUI applies its own folding
// and 8 KiB ingest cap, while this boundary prevents an unbounded peer frame
// from becoming retained transcript state.
const MAX_TOOL_RESULT_CHARS: usize = 8 * 1024;
// ACP peers normally emit very small token chunks. Bound a malformed one before
// it enters the 256-slot event channel; otherwise a sequence of legal 64 MiB
// frames could retain gigabytes while the interactive consumer repaints.
const MAX_STREAM_DELTA_CHARS: usize = 1024 * 1024;
const MAX_SESSION_STATE_ITEMS: usize = 1_024;
const MAX_SESSION_STATE_ID_CHARS: usize = 256;
const MAX_SESSION_STATE_TEXT_CHARS: usize = 4_096;
const MAX_BASH_STREAMS: usize = 1_024;
const MAX_BASH_RENDER_BYTES: usize = 4 * 1024 * 1024;
// Grok Build's published ACP line reader accepts at most 64 MiB including the
// trailing newline. Keep one byte for that delimiter on writes. Reusing this
// conservative ceiling for Kimi also bounds a peer that never terminates a JSON
// line and fits UmaDev's separately bounded attachment set after base64.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTBOUND_BYTES: usize = MAX_FRAME_BYTES - 1;
// Reserve a full MiB for the JSON-RPC/session envelope. Individual input blocks
// are charged by their actual JSON encoding, so escaping and base64 expansion
// cannot turn an accepted 8/20 MiB source attachment into an oversized frame.
const MAX_PROMPT_CONTENT_BYTES: usize = 63 * 1024 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 512;
const DEFAULT_HANDSHAKE_SECS: u64 = 30;
const CONTROL_WRITE_WAIT: Duration = Duration::from_secs(2);
const INTERRUPT_RESPONSE_WAIT: Duration = Duration::from_secs(5);
const INTERJECT_RESPONSE_WAIT: Duration = Duration::from_secs(2);
const SESSION_STATE_RESPONSE_WAIT: Duration = Duration::from_secs(5);
const GROK_SUBAGENT_RESYNC_WAIT: Duration = Duration::from_secs(2);
const GROK_BACKGROUND_CONTROL_WAIT: Duration = Duration::from_secs(5);
const MAX_DEFERRED_FOLDER_TRUST_REQUESTS: usize = 4;
const CLOSE_RESPONSE_WAIT: Duration = Duration::from_millis(750);
const GROK_EOF_GRACE: Duration = Duration::from_secs(6);
const GROK_INTERACTIVE_AUTH_BUDGET: Duration = Duration::from_secs(11 * 60);
// One absolute budget covers cancel, close RPC, stdin EOF, and the official
// Grok post-EOF cleanup tail. If any phase wedges, `end()` leaves this graceful
// window and force-kills/reaps the isolated process tree under
// `END_REAP_BUDGET`; the individual stage timeouts cannot add up unboundedly.
const ACP_GRACEFUL_END_BUDGET: Duration = Duration::from_secs(10);

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, AcpResponseError>>>>>;
type PendingReceiver = oneshot::Receiver<Result<Value, AcpResponseError>>;
type ApprovalMap = Arc<Mutex<HashMap<String, PendingHostRequest>>>;
type SharedWriter = Arc<Mutex<Option<ChildStdin>>>;
type LatestUsage = Arc<Mutex<Option<Usage>>>;
type ActiveSessionId = Arc<RwLock<Option<String>>>;
type ReplaySessionState = Arc<Mutex<ReplaySessionStateAccumulator>>;
type BackgroundProcesses = Arc<Mutex<BackgroundProcessTracker>>;
type ActivePrompt = Arc<RwLock<Option<ActivePromptRecord>>>;
type SessionRoutes = Arc<Mutex<SessionRouteState>>;
type InteractionSessions = Arc<Mutex<HashMap<String, String>>>;
type PromptQueueMirror = Arc<Mutex<GrokPromptQueue>>;
type QueuedPrompts = Arc<Mutex<HashMap<String, u64>>>;
type PendingRunningPrompt = Arc<Mutex<Option<String>>>;
type FolderTrustScopeState = Arc<RwLock<Option<FolderTrustScope>>>;
type DeferredFolderTrustRequests = Arc<Mutex<VecDeque<(Value, Value)>>>;

#[derive(Debug)]
struct AcpResponseError {
    message: String,
    code: Option<i64>,
    /// Grok Build attaches authoritative whole-prompt usage to
    /// `error.data.promptUsage`. Keeping it typed here lets a failed turn retain
    /// real spend without exposing or retaining the rest of the error payload.
    prompt_usage: Option<Box<Usage>>,
}

impl AcpResponseError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            prompt_usage: None,
        }
    }
}

#[derive(Default)]
struct ReplaySessionStateAccumulator {
    model_catalog: Option<SessionStateUpdate>,
    model_transition: Option<SessionStateUpdate>,
    mode: Option<SessionStateUpdate>,
    thinking: Option<SessionStateUpdate>,
    command_catalog: Option<SessionStateUpdate>,
    plan: Option<SessionStateUpdate>,
    prompt_queue: Option<PromptQueueSnapshot>,
}

impl ReplaySessionStateAccumulator {
    fn remember_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::StateUpdate(update) => self.remember(update),
            SessionEvent::SessionModel(model_id) if self.model_transition.is_none() => {
                self.remember(SessionStateUpdate::ModelChanged {
                    model_id,
                    reasoning_effort: None,
                });
            }
            SessionEvent::PromptQueueChanged(snapshot) => self.prompt_queue = Some(snapshot),
            _ => {}
        }
    }

    fn remember(&mut self, update: SessionStateUpdate) {
        match update {
            SessionStateUpdate::ModelCatalogReplaced { .. } => {
                self.model_catalog = Some(update);
            }
            SessionStateUpdate::ModelChanged { .. }
            | SessionStateUpdate::ModelAutoSwitched { .. } => {
                self.model_transition = Some(update);
            }
            SessionStateUpdate::ModeChanged { .. } => self.mode = Some(update),
            SessionStateUpdate::ThinkingChanged { .. } => self.thinking = Some(update),
            SessionStateUpdate::CommandCatalogReplaced { .. } => {
                self.command_catalog = Some(update);
            }
            SessionStateUpdate::PlanReplaced { .. } => self.plan = Some(update),
        }
    }

    fn take_events(&mut self) -> Vec<SessionEvent> {
        let mut events = Vec::with_capacity(7);
        for update in [
            self.model_catalog.take(),
            self.model_transition.take(),
            self.mode.take(),
            self.thinking.take(),
            self.command_catalog.take(),
            self.plan.take(),
        ]
        .into_iter()
        .flatten()
        {
            let legacy_model = match &update {
                SessionStateUpdate::ModelChanged { model_id, .. } => Some(model_id.clone()),
                SessionStateUpdate::ModelAutoSwitched { new_model_id, .. }
                    if !new_model_id.is_empty() =>
                {
                    Some(new_model_id.clone())
                }
                _ => None,
            };
            events.push(SessionEvent::StateUpdate(update));
            if let Some(model_id) = legacy_model {
                events.push(SessionEvent::SessionModel(model_id));
            }
        }
        if let Some(snapshot) = self.prompt_queue.take() {
            events.push(SessionEvent::PromptQueueChanged(snapshot));
        }
        events
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug)]
enum ParsedBackgroundProcess {
    Started {
        raw_task_id: String,
        process: BackgroundProcessInfo,
    },
    Finished {
        raw_task_id: String,
        task_id: String,
        kind: BackgroundProcessKind,
        exit_code: Option<i32>,
        signal: Option<String>,
        truncated: bool,
        will_wake: bool,
    },
}

#[derive(Default)]
struct BackgroundProcessTracker {
    live: HashMap<String, BackgroundProcessInfo>,
    completed: HashSet<String>,
    completed_order: VecDeque<String>,
    replay_observed: bool,
}

impl BackgroundProcessTracker {
    fn apply(&mut self, update: ParsedBackgroundProcess, replaying: bool) -> Option<SessionEvent> {
        if replaying {
            self.replay_observed = true;
        }
        match update {
            ParsedBackgroundProcess::Started {
                raw_task_id,
                process,
            } => {
                if self.completed.contains(&raw_task_id)
                    || self.live.contains_key(&raw_task_id)
                    || self.live.len() >= MAX_BACKGROUND_PROCESSES
                {
                    return None;
                }
                self.live.insert(raw_task_id, process.clone());
                (!replaying).then_some(SessionEvent::BackgroundProcess(
                    BackgroundProcessSignal::Started { process },
                ))
            }
            ParsedBackgroundProcess::Finished {
                raw_task_id,
                task_id,
                kind,
                exit_code,
                signal,
                truncated,
                will_wake,
            } => {
                if !self.remember_completed(&raw_task_id) {
                    return None;
                }
                self.live.remove(&raw_task_id);
                (!replaying).then_some(SessionEvent::BackgroundProcess(
                    BackgroundProcessSignal::Finished {
                        task_id,
                        kind,
                        exit_code,
                        signal,
                        truncated,
                        will_wake,
                    },
                ))
            }
        }
    }

    fn remember_completed(&mut self, task_id: &str) -> bool {
        if self.completed.contains(task_id) {
            return false;
        }
        self.completed.insert(task_id.to_string());
        self.completed_order.push_back(task_id.to_string());
        while self.completed_order.len() > MAX_COMPLETED_BACKGROUND_PROCESSES {
            if let Some(expired) = self.completed_order.pop_front() {
                self.completed.remove(&expired);
            }
        }
        true
    }

    fn take_replay_event(&mut self) -> Option<SessionEvent> {
        if !std::mem::take(&mut self.replay_observed) {
            return None;
        }
        let mut processes = self.live.values().cloned().collect::<Vec<_>>();
        processes.sort_by(|left, right| {
            left.task_id
                .cmp(&right.task_id)
                .then_with(|| left.tool_call_id.cmp(&right.tool_call_id))
        });
        Some(SessionEvent::BackgroundProcess(
            BackgroundProcessSignal::Live { processes },
        ))
    }

    fn reconcile_authoritative_snapshot(&mut self, snapshot: &BackgroundProcessSnapshot) {
        let live_ids = snapshot
            .processes
            .iter()
            .filter(|process| !process.completed)
            .map(|process| process.task_id.as_str())
            .collect::<HashSet<_>>();
        self.live
            .retain(|raw_task_id, _| live_ids.contains(raw_task_id.as_str()));
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone)]
struct ActivePromptRecord {
    prompt_id: String,
    request_id: u64,
}

fn take_active_prompt(
    active: &ActivePrompt,
    expected_prompt_id: &str,
) -> Option<ActivePromptRecord> {
    let mut guard = active.write().ok()?;
    if guard
        .as_ref()
        .is_some_and(|record| record.prompt_id == expected_prompt_id)
    {
        guard.take()
    } else {
        None
    }
}

fn clear_active_prompt(active: &ActivePrompt, expected_prompt_id: &str) {
    let _ = take_active_prompt(active, expected_prompt_id);
}

async fn promote_queued_prompt(
    prompt_id: &str,
    queued_prompts: &QueuedPrompts,
    pending_running_prompt: &PendingRunningPrompt,
    active_prompt: &ActivePrompt,
    turn_active: &AtomicBool,
    session_routes: &SessionRoutes,
    latest_usage: &LatestUsage,
) -> bool {
    let request_id = queued_prompts.lock().await.get(prompt_id).copied();
    let Some(request_id) = request_id else {
        return false;
    };
    if turn_active.load(Ordering::Acquire) {
        *pending_running_prompt.lock().await = Some(prompt_id.to_string());
        return false;
    }
    let promoted = if let Ok(mut active) = active_prompt.write() {
        match active.as_ref() {
            Some(record) if record.prompt_id == prompt_id => true,
            Some(_) => false,
            None => {
                *active = Some(ActivePromptRecord {
                    prompt_id: prompt_id.to_string(),
                    request_id,
                });
                true
            }
        }
    } else {
        false
    };
    if !promoted {
        *pending_running_prompt.lock().await = Some(prompt_id.to_string());
        return false;
    }
    turn_active.store(true, Ordering::Release);
    session_routes.lock().await.begin_turn(prompt_id);
    latest_usage.lock().await.take();
    let mut pending = pending_running_prompt.lock().await;
    if pending.as_deref() == Some(prompt_id) {
        pending.take();
    }
    true
}

async fn promote_pending_queued_prompt(
    queued_prompts: &QueuedPrompts,
    pending_running_prompt: &PendingRunningPrompt,
    active_prompt: &ActivePrompt,
    turn_active: &AtomicBool,
    session_routes: &SessionRoutes,
    latest_usage: &LatestUsage,
) {
    if turn_active.load(Ordering::Acquire) {
        return;
    }
    let prompt_id = pending_running_prompt.lock().await.clone();
    if let Some(prompt_id) = prompt_id {
        let _ = promote_queued_prompt(
            &prompt_id,
            queued_prompts,
            pending_running_prompt,
            active_prompt,
            turn_active,
            session_routes,
            latest_usage,
        )
        .await;
    }
}

/// One ACP-capable base CLI supported by the shared driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcpVendor {
    /// xAI Grok Build CLI (`grok … agent … stdio`).
    Grok,
    /// Moonshot AI Kimi Code CLI (`kimi acp`).
    Kimi,
}

#[derive(Debug)]
struct AcpLaunch {
    args: Vec<String>,
    append_system: Option<String>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default)]
struct NegotiatedCapabilities {
    image: bool,
    embedded_context: bool,
    resume: bool,
    load: bool,
    close: bool,
    interject: bool,
    prompt_queue: bool,
    folder_trust: bool,
    background_process_control: bool,
    set_model: bool,
    set_mode: bool,
    grok_source_contract: bool,
    kimi_source_contract: bool,
}

/// Capabilities implemented by the UmaDev ACP client itself.
///
/// Grok Build's public source treats these flags as an optional reverse-I/O
/// delegation: `terminal=true` makes Grok ask the client to spawn commands, and
/// it switches to client filesystem I/O only when both read and write are true.
/// UmaDev deliberately keeps both delegations disabled. Grok's local fallback
/// then remains inside the already selected `--permission-mode` and `--sandbox`
/// boundary; claiming terminal support here would spawn a second child outside
/// that boundary. This is an honest capability declaration, not a feature loss.
fn acp_client_capabilities(
    vendor: AcpVendor,
    surface: FolderTrustClientSurface,
) -> ClientCapabilities {
    let mut capabilities = ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(false)
            .write_text_file(false))
        .terminal(false);
    if matches!(vendor, AcpVendor::Grok) {
        let mut meta = json!({
            // Published xAI capability metadata: UmaDev consumes incremental
            // tool output and requests colour-free bytes.
            "x.ai/incrementalBashOutput": true,
            "x.ai/bashOutputNoColor": true
        })
        .as_object()
        .cloned()
        .unwrap_or_default();
        // Folder Trust is a Grok extension and must never be advertised to a
        // different ACP vendor merely because it shares this transport core.
        meta.extend(folder_trust_client_capabilities_meta(true, surface));
        capabilities = capabilities.meta(Some(meta));
    }
    capabilities
}

fn grok_initialize_meta() -> Value {
    json!({
        // Grok's public agent reads this exact field and carries it into
        // session ownership/queue attribution. `clientType` is a closed vendor
        // enum, so sending the product name there would only deserialize back
        // to Generic.
        "clientIdentifier": "umadev",
        "clientVersion": env!("CARGO_PKG_VERSION")
    })
}

fn grok_session_meta(rules: Option<&str>, model: Option<&str>) -> Value {
    let mut meta = json!({"clientIdentifier": "umadev"});
    if let Some(rules) = rules.filter(|value| !value.trim().is_empty()) {
        meta["rules"] = Value::String(rules.to_string());
    }
    if let Some(model) = model.filter(|value| !value.trim().is_empty()) {
        meta["modelId"] = Value::String(model.to_string());
    }
    meta
}

fn grok_prompt_id(request_id: u64) -> String {
    format!("umadev-{}-{request_id}", std::process::id())
}

fn grok_prompt_meta(permissions: BasePermissionProfile, request_id: u64) -> Value {
    json!({
        "promptId": grok_prompt_id(request_id),
        "clientIdentifier": "umadev",
        // Grok reconciles its PlanModeTracker from this per-turn field; it is
        // the reliable lifecycle signal even though its session/new response
        // currently omits advertised modes.
        "mode": match permissions {
            BasePermissionProfile::Plan => "plan",
            BasePermissionProfile::Guarded | BasePermissionProfile::Auto => "agent",
        }
    })
}

impl AcpVendor {
    /// Stable UmaDev backend id.
    #[must_use]
    pub const fn backend_id(self) -> &'static str {
        match self {
            Self::Grok => "grok-build",
            Self::Kimi => "kimi-code",
        }
    }

    /// Human-facing base name.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Grok => "Grok Build CLI",
            Self::Kimi => "Kimi Code CLI",
        }
    }

    /// Environment override for the executable.
    #[must_use]
    pub const fn program_env(self) -> &'static str {
        match self {
            Self::Grok => "UMADEV_GROK_BIN",
            Self::Kimi => "UMADEV_KIMI_BIN",
        }
    }

    /// Canonical login command shown to the user. UmaDev never invokes it.
    #[must_use]
    pub const fn login_hint(self) -> &'static str {
        match self {
            Self::Grok => "grok login",
            Self::Kimi => "kimi login",
        }
    }

    /// Canonical install command shown to the user.
    #[must_use]
    pub const fn install_hint(self) -> &'static str {
        self.install_hint_for(cfg!(target_os = "windows"))
    }

    const fn install_hint_for(self, windows: bool) -> &'static str {
        match (self, windows) {
            (Self::Grok, true) => "irm https://x.ai/cli/install.ps1 | iex",
            (Self::Grok, false) => "curl -fsSL https://x.ai/cli/install.sh | bash",
            // Unpinned on purpose: the probe accepts ANY installed Kimi Code that
            // answers `--version` (the audited version only gates the enhanced
            // capabilities), so a user who is genuinely missing the CLI should be
            // sent to the CURRENT release, never a stale pinned one.
            (Self::Kimi, _) => "npm install -g @moonshot-ai/kimi-code",
        }
    }

    fn primary_program(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Kimi => "kimi",
        }
    }

    fn args(
        self,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
    ) -> Vec<String> {
        match self {
            Self::Grok => {
                let mode = permission_mode(permissions, "plan", "default", "bypassPermissions");
                let mut args = vec![
                    "--no-auto-update".to_string(),
                    "--cwd".to_string(),
                    workspace.to_string_lossy().into_owned(),
                    "--permission-mode".to_string(),
                    mode.to_string(),
                ];
                // `--sandbox` is a PagerArgs startup option and is applied
                // before Command::Agent is dispatched. Pin it explicitly so a
                // user's environment/config cannot silently restrict the two
                // profiles whose runtime contract is full access. Grok's
                // higher-priority enterprise requirement may still override
                // this CLI selection by design.
                args.extend([
                    "--sandbox".to_string(),
                    if matches!(permissions, BasePermissionProfile::Plan) {
                        "read-only"
                    } else {
                        "off"
                    }
                    .to_string(),
                ]);
                // These are AgentArgs fields. They must follow the `agent`
                // subcommand and precede its `stdio` mode; placing them at the
                // PagerArgs level silently leaves AgentConfig unchanged.
                args.push("agent".to_string());
                // The official CLI otherwise inherits `[cli] use_leader` from
                // the user's config and may bridge to an already-running
                // executor. That would make this process's sandbox flags an
                // unreliable privilege boundary. UmaDev sessions are direct
                // and single-writer, so opt out explicitly in every profile.
                args.push("--no-leader".to_string());
                if !model.trim().is_empty() {
                    args.extend(["--model".to_string(), model.to_string()]);
                }
                if matches!(permissions, BasePermissionProfile::Auto) {
                    args.push("--always-approve".to_string());
                }
                args.push("stdio".to_string());
                args
            }
            Self::Kimi => vec!["acp".to_string()],
        }
    }

    fn launch(
        self,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        append_system: Option<&str>,
    ) -> AcpLaunch {
        // Firmware never enters argv. Grok consumes it through creation-only
        // session metadata; Kimi has no ACP system-prompt field, so its caller
        // front-loads the same value onto the first directive.
        let append_system = append_system
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        AcpLaunch {
            args: self.args(workspace, model, permissions),
            append_system,
        }
    }
}

fn grok_auth_state_from_api_key(value: Option<&OsStr>) -> AuthState {
    if value.is_some_and(|value| !value.is_empty()) {
        return AuthState::LoggedIn;
    }
    // No API key — fall back to the cached browser/device login. Grok writes its
    // OAuth credential to `~/.grok/auth.json` (keyed by the x.ai auth endpoint),
    // exactly as codex (`~/.codex/auth.json`) and opencode (`…/opencode/auth.json`)
    // store theirs. A non-empty JSON object there is a stored login, so a user who
    // authenticated through the browser flow no longer reads as "unverified".
    // Fail-open: absent / empty `{}` / unreadable stays Unknown — never a false
    // LoggedIn.
    if grok_cached_login_present() {
        AuthState::LoggedIn
    } else {
        AuthState::Unknown
    }
}

/// Grok's cached-login credential file: `~/.grok/auth.json`. `None` when no home
/// directory can be derived (fail-open — the probe then stays Unknown).
fn grok_auth_file() -> Option<std::path::PathBuf> {
    crate::home_dir().map(|home| home.join(".grok").join("auth.json"))
}

/// Whether Grok has a stored browser/device login on disk.
fn grok_cached_login_present() -> bool {
    grok_auth_file().is_some_and(|path| credential_file_has_entries(&path))
}

/// Kimi Code's cached-login credential file: `$KIMI_CODE_HOME/credentials/
/// kimi-code.json` (the CLI honours `KIMI_CODE_HOME`, default `~/.kimi-code`). The
/// file is the OAuth token set (`access_token` / `refresh_token` / `expires_at`).
/// `None` when no home directory can be derived (fail-open — probe stays Unknown).
fn kimi_auth_file() -> Option<std::path::PathBuf> {
    let root = std::env::var_os("KIMI_CODE_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| crate::home_dir().map(|home| home.join(".kimi-code")))?;
    Some(root.join("credentials").join("kimi-code.json"))
}

/// Whether Kimi Code has a stored login on disk. The ACP `authenticate(login)`
/// check at session open stays authoritative for actually USING the token; this
/// only makes the picker show a real login instead of "unverified".
fn kimi_cached_login_present() -> bool {
    kimi_auth_file().is_some_and(|path| credential_file_has_entries(&path))
}

/// Pure check shared by the grok/kimi login probes: `path` exists and holds a
/// non-empty JSON object (at least one credential entry). Fail-open: an absent
/// file, an empty `{}`, a non-object, or any read/parse failure returns `false`,
/// so the auth probe never claims a login it cannot actually see.
fn credential_file_has_entries(path: &std::path::Path) -> bool {
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.as_object().map(|object| !object.is_empty()))
        .unwrap_or(false)
}

fn permission_mode(
    profile: BasePermissionProfile,
    plan: &'static str,
    guarded: &'static str,
    auto: &'static str,
) -> &'static str {
    match profile {
        BasePermissionProfile::Plan => plan,
        BasePermissionProfile::Guarded => guarded,
        BasePermissionProfile::Auto => auto,
    }
}

/// Legacy one-shot plus streaming wrapper backed by a fresh ACP session.
#[derive(Debug, Clone)]
pub struct AcpDriver {
    vendor: AcpVendor,
    program: Option<String>,
    timeout: Duration,
    permissions: BasePermissionProfile,
    workspace: Option<PathBuf>,
}

impl AcpDriver {
    /// Build the driver for `vendor` with conservative Plan permissions.
    #[must_use]
    pub fn new(vendor: AcpVendor) -> Self {
        Self {
            vendor,
            program: None,
            timeout: crate::worker_timeout_from_env(),
            permissions: BasePermissionProfile::Plan,
            workspace: None,
        }
    }

    /// Override the executable, mainly for protocol fixtures.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = Some(program.into());
        self
    }

    /// Set the access and approval posture.
    #[must_use]
    pub fn with_permissions(mut self, permissions: BasePermissionProfile) -> Self {
        self.permissions = permissions;
        self
    }

    /// Override the total one-shot completion timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    async fn open_session(&self, model: &str) -> Result<AcpSession, SessionError> {
        let workspace = self.workspace.clone().unwrap_or_else(default_workspace);
        match &self.program {
            Some(program) => {
                AcpSession::start_with_program(
                    self.vendor,
                    program,
                    &workspace,
                    model,
                    self.permissions,
                )
                .await
            }
            None => AcpSession::start(self.vendor, &workspace, model, self.permissions).await,
        }
    }

    async fn complete_inner(
        &self,
        req: CompletionRequest,
        on_event: Option<&(dyn Fn(StreamEvent) + Send + Sync)>,
    ) -> Result<CompletionResponse, RuntimeError> {
        let requested_model = req.model.clone();
        let prompt = merge_prompt(&req);
        let mut session = self
            .open_session(&requested_model)
            .await
            .map_err(|e| RuntimeError::HostProcess(e.to_string()))?;
        session
            .send_turn(prompt)
            .await
            .map_err(|e| RuntimeError::HostProcess(e.to_string()))?;

        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut state = CompletionState::default();
        let final_status = loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let _ = session.interrupt().await;
                let _ = session.end().await;
                return Err(RuntimeError::Timeout(
                    self.timeout.as_secs(),
                    format!("{} ACP prompt", self.vendor.display_name()),
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let event = match tokio::time::timeout(remaining, session.next_event()).await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    break TurnStatus::Failed("ACP process closed before turn completion".into());
                }
                Err(_) => {
                    let _ = session.interrupt().await;
                    let _ = session.end().await;
                    return Err(RuntimeError::Timeout(
                        self.timeout.as_secs(),
                        format!("{} ACP prompt", self.vendor.display_name()),
                    ));
                }
            };
            if let Some(status) = consume_completion_event(
                event,
                &mut state,
                &mut session,
                self.permissions,
                on_event,
            )
            .await?
            {
                break status;
            }
        };
        let _ = session.end().await;
        match final_status {
            TurnStatus::Completed | TurnStatus::Truncated => Ok(CompletionResponse {
                text: state.text,
                id: format!("{}-acp", self.vendor.backend_id()),
                model: state.model.unwrap_or_else(|| {
                    if requested_model.trim().is_empty() {
                        "unknown".to_string()
                    } else {
                        requested_model
                    }
                }),
                usage: state.usage,
            }),
            TurnStatus::Interrupted => Err(RuntimeError::HostProcess(
                "ACP prompt was interrupted".to_string(),
            )),
            TurnStatus::Failed(reason) => Err(RuntimeError::HostProcess(reason)),
        }
    }
}

#[derive(Default)]
struct CompletionState {
    text: String,
    model: Option<String>,
    usage: Usage,
}

fn consume_completion_presentation(
    event: SessionEvent,
    state: &mut CompletionState,
    on_event: Option<&(dyn Fn(StreamEvent) + Send + Sync)>,
) -> Option<SessionEvent> {
    let Some(callback) = on_event else {
        return match event {
            SessionEvent::TextDelta(delta) => {
                state.text.push_str(&delta);
                None
            }
            SessionEvent::SessionModel(model) => {
                state.model = Some(model);
                None
            }
            SessionEvent::ThinkingDelta(_)
            | SessionEvent::StateUpdate(_)
            | SessionEvent::BackgroundTask(_)
            | SessionEvent::BackgroundProcess(_)
            | SessionEvent::PromptQueueChanged(_)
            | SessionEvent::ToolCall { .. }
            | SessionEvent::ToolCallCorrelated { .. }
            | SessionEvent::ToolProgressCorrelated { .. }
            | SessionEvent::ToolOutputDelta(_)
            | SessionEvent::ToolOutputDeltaCorrelated { .. }
            | SessionEvent::ToolOutputSnapshot(_)
            | SessionEvent::ToolOutputSnapshotCorrelated { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::ToolResultCorrelated { .. } => None,
            event => Some(event),
        };
    };
    match event {
        SessionEvent::TextDelta(delta) => {
            state.text.push_str(&delta);
            callback(StreamEvent::Text { delta });
        }
        SessionEvent::ThinkingDelta(delta) => callback(StreamEvent::ThinkingDelta(delta)),
        SessionEvent::SessionModel(model) => state.model = Some(model),
        SessionEvent::StateUpdate(_)
        | SessionEvent::BackgroundTask(_)
        | SessionEvent::BackgroundProcess(_)
        | SessionEvent::PromptQueueChanged(_) => {}
        SessionEvent::ToolCall { name, input } => {
            callback(StreamEvent::tool_use(name, tool_target(&input)));
        }
        SessionEvent::ToolCallCorrelated {
            call_id,
            name,
            input,
        } => callback(StreamEvent::ToolUseCorrelated {
            call_id,
            name,
            detail: tool_target(&input),
            edit: None,
        }),
        SessionEvent::ToolProgressCorrelated { call_id, title } => {
            callback(StreamEvent::ToolProgressCorrelated { call_id, title });
        }
        SessionEvent::ToolOutputDelta(delta) => callback(StreamEvent::ToolOutputDelta { delta }),
        SessionEvent::ToolOutputDeltaCorrelated { call_id, delta } => {
            callback(StreamEvent::ToolOutputDeltaCorrelated { call_id, delta });
        }
        SessionEvent::ToolOutputSnapshot(output) => {
            callback(StreamEvent::ToolOutputSnapshot { output });
        }
        SessionEvent::ToolOutputSnapshotCorrelated { call_id, output } => {
            callback(StreamEvent::ToolOutputSnapshotCorrelated { call_id, output });
        }
        SessionEvent::ToolResult { ok, summary } => {
            callback(StreamEvent::ToolResult { ok, summary });
        }
        SessionEvent::ToolResultCorrelated {
            call_id,
            ok,
            summary,
        } => callback(StreamEvent::ToolResultCorrelated {
            call_id,
            ok,
            summary,
        }),
        event => return Some(event),
    }
    None
}

async fn consume_completion_event(
    event: SessionEvent,
    state: &mut CompletionState,
    session: &mut AcpSession,
    permissions: BasePermissionProfile,
    on_event: Option<&(dyn Fn(StreamEvent) + Send + Sync)>,
) -> Result<Option<TurnStatus>, RuntimeError> {
    let Some(event) = consume_completion_presentation(event, state, on_event) else {
        return Ok(None);
    };
    match event {
        SessionEvent::NeedApproval { req_id, .. } => {
            session
                .respond(&req_id, ApprovalDecision::Deny)
                .await
                .map_err(|error| RuntimeError::HostProcess(error.to_string()))?;
        }
        SessionEvent::HostRequest { req_id, request } => {
            let response = one_shot_host_response(&request, permissions);
            session
                .respond_host(&req_id, response)
                .await
                .map_err(|error| RuntimeError::HostProcess(error.to_string()))?;
        }
        SessionEvent::TurnDone {
            status,
            usage: reported,
        } => {
            if let Some(reported) = reported {
                state.usage = reported;
            }
            return Ok(Some(status));
        }
        _ => {}
    }
    Ok(None)
}

fn one_shot_host_response(
    request: &HostRequest,
    _permissions: BasePermissionProfile,
) -> HostResponse {
    request.safe_rejection("one-shot ACP runtime has no interactive host surface")
}

#[async_trait]
impl Runtime for AcpDriver {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Openai
    }

    fn capabilities(&self) -> BrainCapabilities {
        BrainCapabilities {
            streaming: true,
            ..BrainCapabilities::default()
        }
    }

    fn fork(&self) -> Option<Box<dyn Runtime>> {
        Some(Box::new(self.clone()))
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, RuntimeError> {
        self.complete_inner(req, None).await
    }

    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        on_event: &(dyn Fn(StreamEvent) + Send + Sync),
    ) -> Result<CompletionResponse, RuntimeError> {
        self.complete_inner(req, Some(on_event)).await
    }
}

#[async_trait]
impl HostDriver for AcpDriver {
    fn backend_id(&self) -> &'static str {
        self.vendor.backend_id()
    }

    fn display_name(&self) -> &'static str {
        self.vendor.display_name()
    }

    fn permission_profile(&self) -> BasePermissionProfile {
        self.permissions
    }

    fn set_session_id(&mut self, _session_id: Option<String>) {
        // ACP peers allocate the only valid id in `session/new`; treating an
        // externally generated UUID as resumable would make the very first
        // legacy completion issue a resume/load for a session that cannot
        // exist. Continuous ACP conversations use one resident `AcpSession`.
    }

    fn set_workspace(&mut self, workspace: PathBuf) {
        self.workspace = Some(workspace);
    }

    fn install_hint(&self) -> Option<&'static str> {
        Some(self.vendor.install_hint())
    }

    fn login_hint(&self) -> Option<&'static str> {
        Some(self.vendor.login_hint())
    }

    async fn probe(&self) -> ProbeResult {
        let program = match &self.program {
            Some(program) => match resolve_and_validate_vendor_program(self.vendor, program) {
                Ok(program) => program,
                Err(error) => {
                    return ProbeResult::Unhealthy {
                        detail: error.to_string(),
                    };
                }
            },
            None => match resolve_vendor_program(self.vendor).await {
                Ok(program) => program,
                Err(_) => {
                    return ProbeResult::NotInstalled {
                        program: self.vendor.primary_program().to_string(),
                    };
                }
            },
        };
        // Any installed version that answers `--version` probes Ready — an
        // unaudited Kimi is no longer marked Unhealthy in the picker. It runs on the
        // baseline ACP contract (the enhanced source-verified capabilities stay
        // gated behind the audited version), matching how Grok already degrades.
        match version_output(&program).await {
            Some(version) => ProbeResult::Ready {
                version,
                auth_state: self.probe_auth().await,
            },
            None => ProbeResult::Unhealthy {
                detail: format!("{} did not return a version", self.vendor.display_name()),
            },
        }
    }

    async fn probe_auth(&self) -> AuthState {
        match self.vendor {
            AcpVendor::Grok => {
                let value = std::env::var_os("XAI_API_KEY");
                grok_auth_state_from_api_key(value.as_deref())
            }
            // Kimi keeps its OAuth credential under its own data root. The ACP
            // `authenticate(login)` check at session open remains authoritative for
            // USING the token, but a stored credential on disk is enough for the
            // picker to show a real login instead of "unverified". Fail-open:
            // absent/empty/unreadable stays Unknown — never a false LoggedIn.
            AcpVendor::Kimi => {
                if kimi_cached_login_present() {
                    AuthState::LoggedIn
                } else {
                    AuthState::Unknown
                }
            }
        }
    }
}

/// A live ACP v1 session spanning multiple UmaDev turns.
pub struct AcpSession {
    vendor: AcpVendor,
    permissions: BasePermissionProfile,
    program: String,
    workspace: PathBuf,
    model: String,
    append_system: Option<String>,
    writer: SharedWriter,
    events: mpsc::Receiver<SessionEvent>,
    event_tx: mpsc::Sender<SessionEvent>,
    pending: PendingMap,
    approvals: ApprovalMap,
    next_id: AtomicU64,
    session_id: String,
    active_session_id: ActiveSessionId,
    active_prompt: ActivePrompt,
    child: std::sync::Mutex<tokio::process::Child>,
    #[cfg(windows)]
    process_job: Option<umadev_process::KillOnCloseJob>,
    stderr: StderrTail,
    stderr_drain: StderrDrain,
    reader_task: Option<tokio::task::JoinHandle<()>>,
    turn_active: Arc<AtomicBool>,
    latest_usage: LatestUsage,
    negotiated: NegotiatedCapabilities,
    grok_source_contract: Arc<AtomicBool>,
    handshake_in_progress: Arc<AtomicBool>,
    fresh_session_bind_in_progress: Arc<AtomicBool>,
    session_routes: SessionRoutes,
    interaction_sessions: InteractionSessions,
    replay_session_state: ReplaySessionState,
    background_processes: BackgroundProcesses,
    queued_prompts: QueuedPrompts,
    pending_running_prompt: PendingRunningPrompt,
    folder_trust_surface: FolderTrustClientSurface,
    folder_trust_scope: FolderTrustScopeState,
    deferred_folder_trust_requests: DeferredFolderTrustRequests,
    deferred_events: VecDeque<SessionEvent>,
}

impl AcpSession {
    /// Start a fresh ACP session, resolving the vendor's canonical executable.
    pub async fn start(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
    ) -> Result<Self, SessionError> {
        Self::start_or_resume(vendor, workspace, model, permissions, None).await
    }

    /// Start a fresh ACP session under an explicit pre-session interaction policy.
    ///
    /// A non-interactive Grok open can return a typed authentication offer.
    /// A user-authorized retry always starts a new child, re-runs initialize,
    /// and revalidates the selected exact method before interactive auth begins.
    pub async fn start_with_policy(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        policy: SessionOpenPolicy,
    ) -> Result<Self, SessionOpenError> {
        Self::start_or_resume_with_policy_and_append_system(
            vendor,
            workspace,
            model,
            permissions,
            None,
            None,
            policy,
        )
        .await
    }

    /// Start a fresh ACP session with both a pre-session auth policy and an
    /// explicit Folder Trust interaction surface.
    ///
    /// Only a real resident UI may pass [`FolderTrustClientSurface::Interactive`].
    /// Every compatibility, one-shot, CI, and daemon path remains headless.
    pub async fn start_with_policy_and_surface(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        policy: SessionOpenPolicy,
        surface: FolderTrustClientSurface,
    ) -> Result<Self, SessionOpenError> {
        Self::start_or_resume_with_policy_and_append_system_and_surface(
            vendor,
            workspace,
            model,
            permissions,
            None,
            None,
            policy,
            surface,
        )
        .await
    }

    /// Start or load a session using the vendor's collision-safe executable.
    pub(crate) async fn start_or_resume(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::start_or_resume_with_append_system(
            vendor,
            workspace,
            model,
            permissions,
            resume_session_id,
            None,
        )
        .await
    }

    /// Start or load a session and append vendor-native system rules.
    /// Fresh Grok sessions receive them through `session/new._meta.rules`;
    /// resumed sessions retain the rules persisted with their original prompt.
    pub(crate) async fn start_or_resume_with_append_system(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
        append_system: Option<&str>,
    ) -> Result<Self, SessionError> {
        let program = resolve_vendor_program(vendor).await?;
        let launch = vendor.launch(workspace, model, permissions, append_system);
        Self::start_with_program_args_and_firmware(
            vendor,
            &program,
            launch.args,
            workspace,
            model,
            permissions,
            resume_session_id,
            launch.append_system,
        )
        .await
    }

    /// Start or resume with native rules and an explicit pre-session policy.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_or_resume_with_policy_and_append_system(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
        append_system: Option<&str>,
        policy: SessionOpenPolicy,
    ) -> Result<Self, SessionOpenError> {
        let program = resolve_vendor_program(vendor).await?;
        let launch = vendor.launch(workspace, model, permissions, append_system);
        Self::start_with_program_args_and_firmware_and_policy(
            vendor,
            &program,
            launch.args,
            workspace,
            model,
            permissions,
            resume_session_id,
            launch.append_system,
            policy,
            FolderTrustClientSurface::Headless,
        )
        .await
    }

    /// Start or resume with native rules, typed authentication, and an explicit
    /// Folder Trust client surface.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_or_resume_with_policy_and_append_system_and_surface(
        vendor: AcpVendor,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
        append_system: Option<&str>,
        policy: SessionOpenPolicy,
        surface: FolderTrustClientSurface,
    ) -> Result<Self, SessionOpenError> {
        let program = resolve_vendor_program(vendor).await?;
        let launch = vendor.launch(workspace, model, permissions, append_system);
        Self::start_with_program_args_and_firmware_and_policy(
            vendor,
            &program,
            launch.args,
            workspace,
            model,
            permissions,
            resume_session_id,
            launch.append_system,
            policy,
            surface,
        )
        .await
    }

    /// Start with an explicit executable. Intended for deterministic fixtures
    /// and administrator overrides; normal callers use [`Self::start`]. This
    /// public seam always creates a fresh vendor session. Persistent resume is
    /// deliberately confined to the crate-owned identity/preflight path.
    pub async fn start_with_program(
        vendor: AcpVendor,
        program: &str,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
    ) -> Result<Self, SessionError> {
        Self::start_with_program_and_append_system(
            vendor,
            program,
            workspace,
            model,
            permissions,
            None,
        )
        .await
    }

    /// Start an explicit executable with vendor-native system rules.
    /// This keeps administrator overrides and hermetic fixtures on the same
    /// launch policy as the canonical executable. Like [`Self::start_with_program`],
    /// this public seam cannot load an opaque persisted session id.
    pub async fn start_with_program_and_append_system(
        vendor: AcpVendor,
        program: &str,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        append_system: Option<&str>,
    ) -> Result<Self, SessionError> {
        let launch = vendor.launch(workspace, model, permissions, append_system);
        Self::start_with_program_args_and_firmware(
            vendor,
            program,
            launch.args,
            workspace,
            model,
            permissions,
            None,
            launch.append_system,
        )
        .await
    }

    /// Start an explicit executable under an explicit pre-session policy.
    ///
    /// This seam exists for administrator overrides and hermetic protocol
    /// fixtures. It performs the same target validation and process isolation
    /// as [`Self::start_with_policy`].
    pub async fn start_with_program_and_policy(
        vendor: AcpVendor,
        program: &str,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        append_system: Option<&str>,
        policy: SessionOpenPolicy,
    ) -> Result<Self, SessionOpenError> {
        let launch = vendor.launch(workspace, model, permissions, append_system);
        Self::start_with_program_args_and_firmware_and_policy(
            vendor,
            program,
            launch.args,
            workspace,
            model,
            permissions,
            None,
            launch.append_system,
            policy,
            FolderTrustClientSurface::Headless,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn start_with_program_args(
        vendor: AcpVendor,
        program: &str,
        args: Vec<String>,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
    ) -> Result<Self, SessionError> {
        Self::start_with_program_args_and_firmware(
            vendor,
            program,
            args,
            workspace,
            model,
            permissions,
            resume_session_id,
            None,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn start_with_program_args_and_policy(
        vendor: AcpVendor,
        program: &str,
        args: Vec<String>,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        policy: SessionOpenPolicy,
    ) -> Result<Self, SessionOpenError> {
        Self::start_with_program_args_and_firmware_and_policy(
            vendor,
            program,
            args,
            workspace,
            model,
            permissions,
            None,
            None,
            policy,
            FolderTrustClientSurface::Headless,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with_program_args_and_firmware(
        vendor: AcpVendor,
        program: &str,
        args: Vec<String>,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
        append_system: Option<String>,
    ) -> Result<Self, SessionError> {
        Self::start_with_program_args_and_firmware_and_policy(
            vendor,
            program,
            args,
            workspace,
            model,
            permissions,
            resume_session_id,
            append_system,
            SessionOpenPolicy::NonInteractive,
            FolderTrustClientSurface::Headless,
        )
        .await
        .map_err(legacy_session_open_error)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn start_with_program_args_and_firmware_and_policy(
        vendor: AcpVendor,
        program: &str,
        args: Vec<String>,
        workspace: &Path,
        model: &str,
        permissions: BasePermissionProfile,
        resume_session_id: Option<&str>,
        append_system: Option<String>,
        policy: SessionOpenPolicy,
        folder_trust_surface: FolderTrustClientSurface,
    ) -> Result<Self, SessionOpenError> {
        if !workspace.is_absolute() {
            return Err(SessionOpenError::from(SessionError::Start(
                "ACP workspace must be an absolute path".to_string(),
            )));
        }
        // Validate the final PATH-resolved target, not the caller's spelling.
        // On Windows a bare `grok` can resolve to the npm `grok.cmd` shim; if
        // validation happened first, ACP arguments would later re-enter
        // `cmd.exe` parsing through `spawn_parts`.
        let program = resolve_and_validate_vendor_program(vendor, program)?;
        let (spawn_program, lead) = spawn_parts(&program);
        let mut cmd = Command::new(spawn_program);
        cmd.args(lead);
        cmd.args(&args);
        cmd.current_dir(workspace);
        cmd.envs(govern_root_env(workspace));
        if matches!(vendor, AcpVendor::Kimi) {
            // The child is protocol infrastructure, not an interactive update
            // surface. Suppress update prompts/checks that could pollute stdio;
            // users update Kimi through its own CLI outside this session.
            cmd.env("KIMI_CODE_NO_AUTO_UPDATE", "1");
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        isolate_process_tree(&mut cmd);
        let mut child = spawn_retrying_etxtbsy(&mut cmd).map_err(|error| {
            SessionError::Start(format!(
                "failed to spawn {} ACP process: {error}",
                vendor.display_name()
            ))
        })?;
        #[cfg(windows)]
        let process_job = umadev_process::KillOnCloseJob::attach(&child);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SessionError::Start("ACP child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SessionError::Start("ACP child has no stdout".to_string()))?;
        let stderr_tail = StderrTail::new();
        let stderr_drain = child
            .stderr
            .take()
            .map_or_else(StderrDrain::empty, |stderr| {
                StderrDrain::spawn(stderr, stderr_tail.clone())
            });

        let writer = Arc::new(Mutex::new(Some(stdin)));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let approvals: ApprovalMap = Arc::new(Mutex::new(HashMap::new()));
        let turn_active = Arc::new(AtomicBool::new(false));
        let grok_source_contract = Arc::new(AtomicBool::new(false));
        // Fresh sessions have no replay window. Recovery flips this only for
        // the bounded resume/load request and clears it on every result path.
        let handshake_in_progress = Arc::new(AtomicBool::new(false));
        let fresh_session_bind_in_progress = Arc::new(AtomicBool::new(false));
        let session_routes: SessionRoutes = Arc::new(Mutex::new(SessionRouteState::default()));
        let interaction_sessions: InteractionSessions = Arc::new(Mutex::new(HashMap::new()));
        let replay_session_state: ReplaySessionState =
            Arc::new(Mutex::new(ReplaySessionStateAccumulator::default()));
        let background_processes: BackgroundProcesses =
            Arc::new(Mutex::new(BackgroundProcessTracker::default()));
        let prompt_queue: PromptQueueMirror = Arc::new(Mutex::new(GrokPromptQueue::default()));
        let queued_prompts: QueuedPrompts = Arc::new(Mutex::new(HashMap::new()));
        let pending_running_prompt: PendingRunningPrompt = Arc::new(Mutex::new(None));
        let folder_trust_scope: FolderTrustScopeState = Arc::new(RwLock::new(None));
        let deferred_folder_trust_requests: DeferredFolderTrustRequests =
            Arc::new(Mutex::new(VecDeque::new()));
        let active_session_id: ActiveSessionId = Arc::new(RwLock::new(None));
        let active_prompt: ActivePrompt = Arc::new(RwLock::new(None));
        let latest_usage: LatestUsage = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel(EVENT_CHANNEL_CAP);
        let reader_context = ReaderContext {
            vendor,
            writer: Arc::clone(&writer),
            pending: Arc::clone(&pending),
            approvals: Arc::clone(&approvals),
            turn_active: Arc::clone(&turn_active),
            active_session_id: Arc::clone(&active_session_id),
            active_prompt: Arc::clone(&active_prompt),
            latest_usage: Arc::clone(&latest_usage),
            event_tx: event_tx.clone(),
            permissions,
            grok_source_contract: Arc::clone(&grok_source_contract),
            handshake_in_progress: Arc::clone(&handshake_in_progress),
            fresh_session_bind_in_progress: Arc::clone(&fresh_session_bind_in_progress),
            session_routes: Arc::clone(&session_routes),
            interaction_sessions: Arc::clone(&interaction_sessions),
            replay_session_state: Arc::clone(&replay_session_state),
            background_processes: Arc::clone(&background_processes),
            prompt_queue: Arc::clone(&prompt_queue),
            queued_prompts: Arc::clone(&queued_prompts),
            pending_running_prompt: Arc::clone(&pending_running_prompt),
            folder_trust_scope: Arc::clone(&folder_trust_scope),
            deferred_folder_trust_requests: Arc::clone(&deferred_folder_trust_requests),
            folder_trust_surface,
        };
        let reader_task = tokio::spawn(reader_loop(stdout, reader_context));

        let mut session = Self {
            vendor,
            permissions,
            program: program.clone(),
            workspace: workspace.to_path_buf(),
            model: model.to_string(),
            append_system,
            writer,
            events: event_rx,
            event_tx,
            pending,
            approvals,
            next_id: AtomicU64::new(1),
            session_id: String::new(),
            active_session_id,
            active_prompt,
            child: std::sync::Mutex::new(child),
            #[cfg(windows)]
            process_job,
            stderr: stderr_tail,
            stderr_drain,
            reader_task: Some(reader_task),
            turn_active,
            latest_usage,
            negotiated: NegotiatedCapabilities::default(),
            grok_source_contract,
            handshake_in_progress,
            fresh_session_bind_in_progress,
            session_routes,
            interaction_sessions,
            replay_session_state,
            background_processes,
            queued_prompts,
            pending_running_prompt,
            folder_trust_surface,
            folder_trust_scope,
            deferred_folder_trust_requests,
            deferred_events: VecDeque::new(),
        };
        match session.handshake(resume_session_id, &policy).await {
            Ok(()) => Ok(session),
            Err(error) => {
                session.abort_opening_process().await;
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn handshake(
        &mut self,
        resume_session_id: Option<&str>,
        policy: &SessionOpenPolicy,
    ) -> Result<(), SessionOpenError> {
        let initialize = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(acp_client_capabilities(
                self.vendor,
                self.folder_trust_surface,
            ))
            .client_info(Implementation::new("umadev", env!("CARGO_PKG_VERSION")).title("UmaDev"));
        let initialize = if matches!(self.vendor, AcpVendor::Grok) {
            initialize.meta(grok_initialize_meta().as_object().cloned())
        } else {
            initialize
        };
        let params = serde_json::to_value(initialize)
            .map_err(|e| SessionError::Start(format!("encode ACP initialize: {e}")))?;
        let initialized = self
            .request_bounded("initialize", params, "initialize")
            .await?;
        validate_initialize(self.vendor, &initialized)?;
        self.negotiated = negotiated_capabilities(self.vendor, &initialized);
        self.grok_source_contract
            .store(self.negotiated.grok_source_contract, Ordering::Release);
        if self.negotiated.grok_source_contract {
            if let Some(update) = parse_initialize_model_catalog(&initialized) {
                self.deferred_events
                    .push_back(SessionEvent::StateUpdate(update));
            }
            if let Some(update) = parse_initialize_command_catalog(&initialized) {
                self.deferred_events
                    .push_back(SessionEvent::StateUpdate(update));
            }
        }

        let grok_auth_offer = match self.vendor {
            AcpVendor::Grok if self.negotiated.grok_source_contract => {
                validate_grok_auth_gate(&initialized)?;
                self.apply_grok_auth_policy(&initialized, policy).await?
            }
            AcpVendor::Grok => {
                if matches!(policy, SessionOpenPolicy::UserAuthorized { .. }) {
                    return Err(SessionError::Start(
                        "the fresh Grok Build child did not prove the pinned source contract; interactive authentication was not started"
                            .to_string(),
                    )
                    .into());
                }
                if let Some(method_id) = safe_grok_auth_method(&initialized, false) {
                    let _ = self
                        .request_bounded(
                            "authenticate",
                            json!({"methodId": method_id, "_meta":{"headless":true}}),
                            "non-interactive authentication",
                        )
                        .await?;
                } else if advertised_auth_methods(&initialized).is_some() {
                    return Err(SessionError::Start(
                        "Grok Build advertised no usable non-interactive authentication"
                            .to_string(),
                    )
                    .into());
                }
                None
            }
            AcpVendor::Kimi => {
                self.verify_kimi_auth(&initialized, policy).await?;
                None
            }
        };

        let setup = if let Some(resume_id) = resume_session_id.filter(|id| !id.trim().is_empty()) {
            // Lock attribution before load/resume: Grok replays history before
            // returning the RPC response, and no other session may inject
            // authority-bearing frames during that window.
            if let Ok(mut active) = self.active_session_id.write() {
                *active = Some(resume_id.to_string());
            }
            self.bind_folder_trust_scope(resume_id)?;
            self.session_routes
                .lock()
                .await
                .begin_replay(resume_id)
                .map_err(|_| {
                    SessionError::Start("invalid ACP session id for replay".to_string())
                })?;
            let (method, params) = if recovery_method(self.negotiated) == Some("session/resume") {
                let request =
                    ResumeSessionRequest::new(resume_id.to_string(), self.workspace.clone());
                let request = if matches!(self.vendor, AcpVendor::Grok) {
                    request.meta(grok_session_meta(None, None).as_object().cloned())
                } else {
                    request
                };
                let params = serde_json::to_value(request)
                    .map_err(|e| SessionError::Start(format!("encode ACP session/resume: {e}")))?;
                ("session/resume", params)
            } else if recovery_method(self.negotiated) == Some("session/load") {
                let request =
                    LoadSessionRequest::new(resume_id.to_string(), self.workspace.clone());
                let request = if matches!(self.vendor, AcpVendor::Grok) {
                    request.meta(grok_session_meta(None, None).as_object().cloned())
                } else {
                    request
                };
                let params = serde_json::to_value(request)
                    .map_err(|e| SessionError::Start(format!("encode ACP session/load: {e}")))?;
                ("session/load", params)
            } else {
                return Err(SessionError::Start(format!(
                    "{} advertises neither ACP session/resume nor session/load",
                    self.vendor.display_name()
                ))
                .into());
            };
            self.handshake_in_progress.store(true, Ordering::Release);
            let load_result = self.request_bounded(method, params, method).await;
            let result = match load_result {
                Ok(result) => result,
                Err(error) => {
                    self.session_routes.lock().await.clear_failed_replay();
                    self.replay_session_state.lock().await.clear();
                    self.background_processes.lock().await.clear();
                    self.handshake_in_progress.store(false, Ordering::Release);
                    if let Ok(mut scope) = self.folder_trust_scope.write() {
                        scope.take();
                    }
                    return Err(error.into());
                }
            };
            self.session_routes.lock().await.commit_replay();
            self.session_id = resume_id.to_string();
            if let Ok(mut active) = self.active_session_id.write() {
                *active = Some(self.session_id.clone());
            }
            self.bind_folder_trust_scope(&self.session_id)?;
            if self.negotiated.grok_source_contract {
                self.resync_grok_subagent_routes().await;
            }
            self.handshake_in_progress.store(false, Ordering::Release);
            result
        } else {
            // Grok Build's public ACP agent folds `_meta.rules` into a
            // `<human_rules>` section of the native system prompt when the
            // session is created. Rules are creation-only; loading a persisted
            // session intentionally keeps its original system prompt.
            let request = NewSessionRequest::new(self.workspace.clone());
            let request = if matches!(self.vendor, AcpVendor::Grok) {
                request.meta(
                    grok_session_meta(self.append_system.as_deref(), Some(self.model.as_str()))
                        .as_object()
                        .cloned(),
                )
            } else {
                request
            };
            let params = serde_json::to_value(request)
                .map_err(|e| SessionError::Start(format!("encode ACP session/new: {e}")))?;
            self.fresh_session_bind_in_progress
                .store(true, Ordering::Release);
            let result = match self
                .request_bounded_with_rpc_code("session/new", params, "session/new")
                .await
            {
                Ok(result) => result,
                Err((error, Some(-32_000))) => {
                    self.fresh_session_bind_in_progress
                        .store(false, Ordering::Release);
                    self.deferred_folder_trust_requests.lock().await.clear();
                    if let Some(offer) = grok_auth_offer.clone() {
                        return Err(SessionOpenError::AuthRequired(offer));
                    }
                    if matches!(self.vendor, AcpVendor::Kimi) {
                        return Err(SessionError::Start(
                            "Kimi Code rejected the session because login is required. Run `kimi login` explicitly, then retry"
                                .to_string(),
                        )
                        .into());
                    }
                    return Err(error.into());
                }
                Err((error, _)) => {
                    self.fresh_session_bind_in_progress
                        .store(false, Ordering::Release);
                    self.deferred_folder_trust_requests.lock().await.clear();
                    return Err(error.into());
                }
            };
            self.session_id = result
                .get("sessionId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| {
                    SessionError::Start("ACP session/new returned no sessionId".to_string())
                })?
                .to_string();
            if let Ok(mut active) = self.active_session_id.write() {
                *active = Some(self.session_id.clone());
            }
            self.session_routes
                .lock()
                .await
                .activate_root(&self.session_id)
                .map_err(|_| {
                    SessionError::Start("invalid ACP session/new sessionId".to_string())
                })?;
            self.bind_folder_trust_scope(&self.session_id)?;
            self.fresh_session_bind_in_progress
                .store(false, Ordering::Release);
            self.flush_deferred_folder_trust_requests().await;
            result
        };

        // Replay presentation is intentionally suppressed before `start()`
        // returns, but unfinished native subagents remain live state. Stage
        // their bounded start edges locally so `next_event()` can deliver them
        // without deadlocking the 256-slot reader channel during handshake.
        let live_subagents = self.session_routes.lock().await.live_subagent_ids();
        if !live_subagents.is_empty() {
            self.deferred_events.push_back(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Live {
                    agent_ids: live_subagents,
                },
            ));
        }

        let replay_state_events = self.replay_session_state.lock().await.take_events();
        self.deferred_events.extend(replay_state_events);

        if let Some(event) = self.background_processes.lock().await.take_replay_event() {
            self.deferred_events.push_back(event);
        }

        if let Some(update) = parse_setup_model_catalog(&setup) {
            self.negotiated.set_model =
                self.negotiated.grok_source_contract || self.negotiated.kimi_source_contract;
            let current_model_id = match &update {
                SessionStateUpdate::ModelCatalogReplaced {
                    current_model_id, ..
                } => current_model_id.clone(),
                _ => unreachable!("model catalog parser returned a non-catalog update"),
            };
            self.deferred_events
                .push_back(SessionEvent::StateUpdate(update));
            self.deferred_events
                .push_back(SessionEvent::SessionModel(current_model_id));
        } else if let Some(model) = extract_session_model(&setup) {
            self.deferred_events
                .push_back(SessionEvent::SessionModel(model));
        }
        self.apply_requested_kimi_model(&setup).await?;
        self.apply_permission_mode(&setup).await?;
        Ok(())
    }

    async fn verify_kimi_auth(
        &self,
        initialized: &Value,
        policy: &SessionOpenPolicy,
    ) -> Result<(), SessionError> {
        if !matches!(policy, SessionOpenPolicy::NonInteractive) {
            return Err(SessionError::Start(
                "Kimi Code authentication is terminal-owned; run `kimi login` explicitly, then retry. UmaDev does not launch a browser or login subprocess automatically"
                    .to_string(),
            ));
        }
        let login_advertised = advertised_auth_methods(initialized).is_some_and(|methods| {
            methods.iter().any(|method| {
                method
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == "login")
            })
        });
        if !login_advertised {
            return Err(SessionError::Start(
                "Kimi Code did not advertise its audited terminal login method; update Kimi Code or run `kimi doctor`"
                    .to_string(),
            ));
        }
        match self
            .request_bounded_with_rpc_code(
                "authenticate",
                json!({"methodId":"login"}),
                "Kimi authentication check",
            )
            .await
        {
            Ok(_) => Ok(()),
            Err((_, Some(-32_000))) => Err(SessionError::Start(
                "Kimi Code is not logged in. Run `kimi login` in a terminal, complete the login there, then retry; UmaDev will never open the login browser automatically"
                    .to_string(),
            )),
            Err((error, _)) => Err(error),
        }
    }

    async fn apply_requested_kimi_model(&mut self, setup: &Value) -> Result<(), SessionError> {
        if !self.negotiated.kimi_source_contract {
            return Ok(());
        }
        let Some(current) = extract_session_model(setup) else {
            return Err(SessionError::Start(
                "Kimi Code session did not return its audited model config option".to_string(),
            ));
        };
        if self.model.trim().is_empty() {
            self.model = current;
            return Ok(());
        }
        if self.model == current {
            return Ok(());
        }
        let requested = self.model.clone();
        let catalog = parse_setup_model_catalog(setup).ok_or_else(|| {
            SessionError::Start("Kimi Code returned no model catalog".to_string())
        })?;
        let SessionStateUpdate::ModelCatalogReplaced {
            available_models, ..
        } = catalog
        else {
            unreachable!("model catalog parser returned a non-catalog update");
        };
        if !available_models
            .iter()
            .any(|model| model.model_id == requested)
        {
            return Err(SessionError::Start(format!(
                "Kimi Code model `{requested}` is not present in the session model catalog"
            )));
        }
        let response = self
            .request_with_timeout(
                "session/set_config_option",
                json!({
                    "sessionId":self.session_id,
                    "configId":"model",
                    "value":requested
                }),
                SESSION_STATE_RESPONSE_WAIT,
                "Kimi Code model selection timed out",
            )
            .await?;
        if extract_session_model(&response).as_deref() != Some(requested.as_str()) {
            return Err(SessionError::Start(
                "Kimi Code did not confirm the requested model in configOptions".to_string(),
            ));
        }
        self.deferred_events
            .push_back(SessionEvent::SessionModel(requested));
        Ok(())
    }

    fn bind_folder_trust_scope(&self, session_id: &str) -> Result<(), SessionError> {
        if !self.negotiated.folder_trust
            || !matches!(
                self.folder_trust_surface,
                FolderTrustClientSurface::Interactive
            )
        {
            return Ok(());
        }
        let scope = FolderTrustScope::new(session_id.to_string(), self.workspace.clone()).map_err(
            |error| SessionError::Start(format!("bind Grok Folder Trust scope: {error}")),
        )?;
        let mut state = self.folder_trust_scope.write().map_err(|_| {
            SessionError::Start("Grok Folder Trust scope state is unavailable".to_string())
        })?;
        *state = Some(scope);
        Ok(())
    }

    async fn flush_deferred_folder_trust_requests(&self) {
        let deferred = {
            let mut deferred = self.deferred_folder_trust_requests.lock().await;
            std::mem::take(&mut *deferred)
        };
        if deferred.is_empty() {
            return;
        }
        let scope = self
            .folder_trust_scope
            .read()
            .ok()
            .and_then(|scope| scope.clone());
        for (frame, params) in deferred {
            if let Some(scope) = &scope {
                surface_bound_folder_trust_request(
                    &frame,
                    &params,
                    scope,
                    &self.writer,
                    &self.approvals,
                    &self.interaction_sessions,
                    &self.event_tx,
                )
                .await;
            } else {
                let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
                let response =
                    folder_trust_response_frame(&raw_id, FolderTrustUserDecision::KeepGated);
                let _ = write_json_line(&self.writer, &response).await;
            }
        }
    }

    async fn apply_grok_auth_policy(
        &mut self,
        initialized: &Value,
        policy: &SessionOpenPolicy,
    ) -> Result<Option<AuthOffer>, SessionOpenError> {
        if advertised_auth_methods(initialized).is_none() {
            if matches!(policy, SessionOpenPolicy::UserAuthorized { .. }) {
                return Err(SessionError::Start(
                    "the fresh Grok Build initialize response did not advertise the confirmed authentication method"
                        .to_string(),
                )
                .into());
            }
            return Ok(None);
        }

        let offer = GrokAuthCatalog::parse_initialize(initialized)
            .ok()
            .map(|catalog| catalog.offer());
        let attempt_id = policy.attempt_id().unwrap_or_else(|| SessionOpenId::new(0));
        let now = Instant::now();
        let mut attempt = GrokAuthAttempt::new(attempt_id, now + GROK_INTERACTIVE_AUTH_BUDGET);
        let action = match policy {
            SessionOpenPolicy::NonInteractive => attempt.initialized(attempt_id, initialized, now),
            SessionOpenPolicy::UserAuthorized { method_id, .. } => attempt
                .initialized_with_explicit_confirmation(
                    attempt_id,
                    initialized,
                    method_id,
                    GrokInteractiveAuthOptions::initial(false),
                    now,
                ),
        }
        .map_err(grok_auth_attempt_error)?;

        match action {
            GrokAuthAction::PresentOffer(offer) => Err(SessionOpenError::AuthRequired(offer)),
            GrokAuthAction::SessionReady(_) => Ok(offer),
            GrokAuthAction::SendRpc(request) => {
                if request.may_open_browser() {
                    return Err(SessionError::Start(
                        "browser-capable Grok authentication was selected without confirmation"
                            .to_string(),
                    )
                    .into());
                }
                let result = self
                    .request_bounded(
                        request.method(),
                        request.params().clone(),
                        "non-interactive authentication",
                    )
                    .await;
                match result {
                    Ok(response) => {
                        validate_grok_auth_gate(&response)?;
                        let _ = attempt
                            .authentication_succeeded(attempt_id, Instant::now())
                            .map_err(grok_auth_attempt_error)?;
                        Ok(offer)
                    }
                    Err(error) => {
                        let _ = attempt.authentication_failed(attempt_id, Instant::now());
                        Err(error.into())
                    }
                }
            }
            GrokAuthAction::StartInteractive {
                authenticate,
                get_url,
            } => {
                let events = policy.event_sender().ok_or_else(|| {
                    SessionError::Start(
                        "interactive Grok authentication has no event receiver".to_string(),
                    )
                })?;
                self.run_grok_interactive_auth(&mut attempt, authenticate, get_url, events)
                    .await?;
                Ok(offer)
            }
            GrokAuthAction::AbortAndReap { reason, .. } => Err(grok_auth_stop_error(reason).into()),
            GrokAuthAction::PollAuthUrlAfter { .. }
            | GrokAuthAction::PresentChallenge(_)
            | GrokAuthAction::SendSensitiveRpc(_)
            | GrokAuthAction::Ignored(_) => Err(SessionError::Start(
                "invalid Grok authentication bootstrap transition".to_string(),
            )
            .into()),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn run_grok_interactive_auth(
        &mut self,
        attempt: &mut GrokAuthAttempt,
        authenticate: GrokAuthRpc,
        get_url: GrokAuthRpc,
        events: &SessionOpenEventSender,
    ) -> Result<(), SessionOpenError> {
        if !authenticate.may_open_browser() || get_url.may_open_browser() {
            return Err(SessionError::Start(
                "invalid Grok interactive authentication RPC classification".to_string(),
            )
            .into());
        }
        let attempt_id = attempt.attempt_id();
        let (auth_id, mut auth_rx, _) = self
            .begin_request(authenticate.method(), authenticate.params().clone())
            .await?;
        let (mut url_id, mut url_rx, _) = match self
            .begin_request(get_url.method(), get_url.params().clone())
            .await
        {
            Ok(request) => request,
            Err(error) => {
                self.pending.lock().await.remove(&auth_id);
                return Err(error.into());
            }
        };

        let mut control_rx = loop {
            let deadline = tokio::time::Instant::from_std(attempt.deadline());
            let url_result = tokio::select! {
                biased;
                result = &mut auth_rx => {
                    self.pending.lock().await.remove(&url_id);
                    return Self::finish_grok_auth_response(attempt, events, result);
                }
                result = &mut url_rx => result,
                () = events.closed() => {
                    self.pending.lock().await.remove(&auth_id);
                    self.pending.lock().await.remove(&url_id);
                    let action = attempt.event_receiver_closed(attempt_id, Instant::now());
                    return Err(Self::stop_grok_auth(&action, events).into());
                }
                () = tokio::time::sleep_until(deadline) => {
                    self.pending.lock().await.remove(&auth_id);
                    self.pending.lock().await.remove(&url_id);
                    let action = attempt.check_timeout(Instant::now()).unwrap_or_else(|| {
                        attempt.transport_closed(attempt_id, Instant::now())
                    });
                    return Err(Self::stop_grok_auth(&action, events).into());
                }
            };
            let response = match pending_response(url_result) {
                Ok(response) => response,
                Err(error) => {
                    self.pending.lock().await.remove(&auth_id);
                    let action = attempt
                        .authentication_failed(attempt_id, Instant::now())
                        .unwrap_or_else(|_| attempt.transport_closed(attempt_id, Instant::now()));
                    let _ = Self::stop_grok_auth(&action, events);
                    return Err(error.into());
                }
            };
            match attempt
                .observed_auth_url(attempt_id, &response, Instant::now())
                .map_err(grok_auth_attempt_error)?
            {
                GrokAuthAction::PollAuthUrlAfter { delay, request } => {
                    let deadline = tokio::time::Instant::from_std(attempt.deadline());
                    tokio::select! {
                        biased;
                        result = &mut auth_rx => {
                            return Self::finish_grok_auth_response(attempt, events, result);
                        }
                        () = events.closed() => {
                            self.pending.lock().await.remove(&auth_id);
                            let action = attempt.event_receiver_closed(attempt_id, Instant::now());
                            return Err(Self::stop_grok_auth(&action, events).into());
                        }
                        () = tokio::time::sleep_until(deadline) => {
                            self.pending.lock().await.remove(&auth_id);
                            let action = attempt.check_timeout(Instant::now()).unwrap_or_else(|| {
                                attempt.transport_closed(attempt_id, Instant::now())
                            });
                            return Err(Self::stop_grok_auth(&action, events).into());
                        }
                        () = tokio::time::sleep(delay) => {}
                    }
                    let request = self
                        .begin_request(request.method(), request.params().clone())
                        .await?;
                    url_id = request.0;
                    url_rx = request.1;
                }
                GrokAuthAction::PresentChallenge(challenge) => {
                    let mode = challenge.mode();
                    let (control, control_rx) = AuthControl::channel_for_mode(mode);
                    if events
                        .send(SessionOpenEvent::Challenge { challenge, control })
                        .is_err()
                    {
                        self.pending.lock().await.remove(&auth_id);
                        let action = attempt.event_receiver_closed(attempt_id, Instant::now());
                        return Err(Self::stop_grok_auth(&action, events).into());
                    }
                    break control_rx;
                }
                GrokAuthAction::AbortAndReap { reason, settled } => {
                    self.pending.lock().await.remove(&auth_id);
                    let _ = events.send(SessionOpenEvent::Settled(settled));
                    return Err(grok_auth_stop_error(reason).into());
                }
                _ => {
                    self.pending.lock().await.remove(&auth_id);
                    return Err(SessionError::Start(
                        "invalid Grok authentication URL transition".to_string(),
                    )
                    .into());
                }
            }
        };

        let mut submit: Option<(u64, PendingReceiver)> = None;
        loop {
            let deadline = tokio::time::Instant::from_std(attempt.deadline());
            tokio::select! {
                biased;
                result = &mut auth_rx => {
                    if let Some((id, _)) = submit.take() {
                        self.pending.lock().await.remove(&id);
                    }
                    return Self::finish_grok_auth_response(attempt, events, result);
                }
                command = control_rx.recv(), if submit.is_none() => {
                    match command {
                        Some(AuthCommand::Cancel) => {
                            self.pending.lock().await.remove(&auth_id);
                            let action = attempt.cancel(attempt_id, Instant::now());
                            return Err(Self::stop_grok_auth(&action, events).into());
                        }
                        Some(AuthCommand::SubmitCode(code)) => {
                            let action = attempt
                                .submit_code(attempt_id, code, Instant::now())
                                .map_err(grok_auth_attempt_error)?;
                            let GrokAuthAction::SendSensitiveRpc(request) = action else {
                                return Err(SessionError::Start(
                                    "invalid Grok submit-code transition".to_string(),
                                ).into());
                            };
                            submit = Some(self.begin_sensitive_auth_request(&request).await?);
                        }
                        None => {
                            self.pending.lock().await.remove(&auth_id);
                            let action = attempt.event_receiver_closed(attempt_id, Instant::now());
                            return Err(Self::stop_grok_auth(&action, events).into());
                        }
                    }
                }
                result = async {
                    let (_, receiver) = submit.as_mut().expect("guarded by select condition");
                    receiver.await
                }, if submit.is_some() => {
                    submit.take();
                    if let Err(error) = pending_response(result) {
                        self.pending.lock().await.remove(&auth_id);
                        let action = attempt.authentication_failed(attempt_id, Instant::now())
                            .unwrap_or_else(|_| attempt.transport_closed(attempt_id, Instant::now()));
                        let _ = Self::stop_grok_auth(&action, events);
                        return Err(error.into());
                    }
                }
                () = events.closed() => {
                    self.pending.lock().await.remove(&auth_id);
                    if let Some((id, _)) = submit.take() {
                        self.pending.lock().await.remove(&id);
                    }
                    let action = attempt.event_receiver_closed(attempt_id, Instant::now());
                    return Err(Self::stop_grok_auth(&action, events).into());
                }
                () = tokio::time::sleep_until(deadline) => {
                    self.pending.lock().await.remove(&auth_id);
                    if let Some((id, _)) = submit.take() {
                        self.pending.lock().await.remove(&id);
                    }
                    let action = attempt.check_timeout(Instant::now()).unwrap_or_else(|| {
                        attempt.transport_closed(attempt_id, Instant::now())
                    });
                    return Err(Self::stop_grok_auth(&action, events).into());
                }
            }
        }
    }

    async fn begin_sensitive_auth_request(
        &self,
        request: &GrokSensitiveAuthRpc,
    ) -> Result<(u64, PendingReceiver), SessionError> {
        let (id, receiver, _) = self
            .begin_request(request.method(), request.reveal_params().clone())
            .await?;
        Ok((id, receiver))
    }

    fn finish_grok_auth_response(
        attempt: &mut GrokAuthAttempt,
        events: &SessionOpenEventSender,
        result: Result<Result<Value, AcpResponseError>, oneshot::error::RecvError>,
    ) -> Result<(), SessionOpenError> {
        let attempt_id = attempt.attempt_id();
        match pending_response(result) {
            Ok(response) => {
                validate_grok_auth_gate(&response)?;
                let action = attempt
                    .authentication_succeeded(attempt_id, Instant::now())
                    .map_err(grok_auth_attempt_error)?;
                let GrokAuthAction::SessionReady(settled) = action else {
                    return Err(SessionError::Start(
                        "invalid Grok authentication completion transition".to_string(),
                    )
                    .into());
                };
                let _ = events.send(SessionOpenEvent::Settled(settled));
                Ok(())
            }
            Err(error) => {
                let action = attempt
                    .authentication_failed(attempt_id, Instant::now())
                    .unwrap_or_else(|_| attempt.transport_closed(attempt_id, Instant::now()));
                let _ = Self::stop_grok_auth(&action, events);
                Err(error.into())
            }
        }
    }

    fn stop_grok_auth(action: &GrokAuthAction, events: &SessionOpenEventSender) -> SessionError {
        if let GrokAuthAction::AbortAndReap { settled, reason } = action {
            let _ = events.send(SessionOpenEvent::Settled(*settled));
            grok_auth_stop_error(*reason)
        } else {
            SessionError::Start("invalid Grok authentication stop transition".to_string())
        }
    }

    async fn abort_opening_process(&mut self) {
        self.pending.lock().await.clear();
        let _ = tokio::time::timeout(CONTROL_WRITE_WAIT, close_acp_stdin(&self.writer)).await;
        #[cfg(windows)]
        if let Some(job) = self.process_job.take() {
            job.terminate();
            drop(job);
        }
        reap_isolated_process_tree(&self.child, END_REAP_BUDGET).await;
        self.stderr_drain.shutdown().await;
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
    }

    /// Reconcile the replayed lifecycle with Grok's authoritative, transient
    /// parent-scoped live snapshots. The query walks descendants breadth-first
    /// because `list_running` returns only direct children of the requested
    /// parent. Any malformed/unsupported response leaves the replay graph in
    /// place; recovery must never make a healthy session unusable.
    async fn resync_grok_subagent_routes(&self) {
        let mut parents = VecDeque::from([self.session_id.clone()]);
        let mut visited = HashSet::new();
        while let Some(parent) = parents.pop_front() {
            if !visited.insert(parent.clone()) || visited.len() > MAX_REPLAY_SUBAGENTS {
                continue;
            }
            let Ok(result) = self
                .request_with_timeout(
                    "_x.ai/subagent/list_running",
                    json!({"sessionId":parent}),
                    GROK_SUBAGENT_RESYNC_WAIT,
                    "Grok subagent live-state resync timed out",
                )
                .await
            else {
                continue;
            };
            let Ok(children) = self
                .session_routes
                .lock()
                .await
                .reconcile_running(&parent, &result)
            else {
                continue;
            };
            for child in children {
                if visited.len() + parents.len() >= MAX_REPLAY_SUBAGENTS {
                    break;
                }
                parents.push_back(child);
            }
        }
    }

    async fn fetch_grok_background_processes(
        &self,
    ) -> Result<BackgroundProcessSnapshot, SessionError> {
        if !self.negotiated.background_process_control {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::BackgroundProcessControl,
            ));
        }
        if self.session_id.is_empty() {
            return Err(SessionError::Closed);
        }
        let response = self
            .request_with_timeout(
                GROK_TASK_LIST_METHOD,
                grok_task_list_params(&self.session_id),
                GROK_BACKGROUND_CONTROL_WAIT,
                "Grok Build background-process list timed out",
            )
            .await?;
        let mut snapshot = parse_grok_task_list_response(&response, &self.session_id)
            .map_err(|message| SessionError::Send(message.to_string()))?;
        for process in &mut snapshot.processes {
            process.signal = process.signal.take().map(|signal| redact_text(&signal));
        }
        self.background_processes
            .lock()
            .await
            .reconcile_authoritative_snapshot(&snapshot);
        Ok(snapshot)
    }

    async fn apply_permission_mode(&mut self, setup: &Value) -> Result<(), SessionError> {
        let source_contract = match self.vendor {
            AcpVendor::Grok => self.negotiated.grok_source_contract,
            AcpVendor::Kimi => self.negotiated.kimi_source_contract,
        };
        let Some(mode_id) =
            select_session_mode(setup, self.vendor, self.permissions, source_contract)
        else {
            return if matches!(self.vendor, AcpVendor::Kimi) {
                Err(SessionError::Start(
                    "Kimi Code did not advertise the required mode config option".to_string(),
                ))
            } else {
                Ok(())
            };
        };
        // The read-only process sandbox remains Plan's hard permission boundary.
        // The source-identified Grok agent additionally implements
        // session/set_mode even though session/new does not advertise `modes`;
        // set it explicitly so its PlanModeTracker and write guard activate.
        // Errors stay fail-open for older agents because the launch boundary is
        // still authoritative.
        let request = match self.vendor {
            AcpVendor::Grok => {
                self.request_with_timeout(
                    "session/set_mode",
                    json!({"sessionId": self.session_id, "modeId": mode_id}),
                    handshake_timeout(),
                    "ACP session/set_mode timed out",
                )
                .await
            }
            AcpVendor::Kimi => {
                self.request_with_timeout(
                    "session/set_config_option",
                    json!({
                        "sessionId":self.session_id,
                        "configId":"mode",
                        "value":mode_id
                    }),
                    handshake_timeout(),
                    "Kimi Code permission-mode selection timed out",
                )
                .await
            }
        };
        if matches!(self.vendor, AcpVendor::Kimi) {
            let response = request?;
            if extract_config_option_current(&response, "mode") != Some(mode_id.as_str()) {
                return Err(SessionError::Start(
                    "Kimi Code did not confirm the requested permission mode".to_string(),
                ));
            }
            let mode = SessionMode::try_from(mode_id.as_str()).map_err(|_| {
                SessionError::Start(
                    "Kimi Code confirmed a mode outside UmaDev's audited mapping".to_string(),
                )
            })?;
            self.deferred_events.push_back(SessionEvent::StateUpdate(
                SessionStateUpdate::ModeChanged { mode },
            ));
        }
        // Grok's process sandbox/permission flags remain authoritative, so an
        // older peer's optional mode-RPC failure stays fail-open.
        Ok(())
    }

    async fn request_bounded(
        &self,
        method: &str,
        params: Value,
        label: &str,
    ) -> Result<Value, SessionError> {
        match self
            .request_with_timeout(
                method,
                params,
                handshake_timeout(),
                &format!("ACP {label} timed out; check login and CLI version"),
            )
            .await
        {
            Ok(value) => Ok(value),
            Err(SessionError::Send(error)) => {
                Err(SessionError::Start(format!("ACP {label}: {error}")))
            }
            Err(SessionError::Closed) => Err(SessionError::Start(format!(
                "ACP {label}: process closed before responding"
            ))),
            Err(error) => Err(SessionError::Start(format!("ACP {label}: {error}"))),
        }
    }

    async fn request_bounded_with_rpc_code(
        &self,
        method: &str,
        params: Value,
        label: &str,
    ) -> Result<Value, (SessionError, Option<i64>)> {
        let (id, receiver, _) = self
            .begin_request(method, params)
            .await
            .map_err(|error| (error, None))?;
        match tokio::time::timeout(handshake_timeout(), receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err((
                SessionError::Start(format!("ACP {label}: {}", error.message)),
                error.code,
            )),
            Ok(Err(_)) => Err((
                SessionError::Start(format!("ACP {label}: process closed before responding")),
                None,
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err((
                    SessionError::Start(format!(
                        "ACP {label} timed out; check login and CLI version"
                    )),
                    None,
                ))
            }
        }
    }

    async fn begin_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(u64, PendingReceiver, usize), SessionError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(SessionError::Send(
                    "ACP pending-request limit reached".to_string(),
                ));
            }
            pending.insert(id, tx);
        }
        let frame = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        let encoded_bytes = serde_json::to_vec(&frame)
            .map_err(|error| SessionError::Send(format!("encode ACP request frame: {error}")))?
            .len();
        if let Err(error) = write_json_line(&self.writer, &frame).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        Ok((id, rx, encoded_bytes))
    }

    /// Issue a JSON-RPC request and remove its waiter on timeout. Without the
    /// explicit removal, every timed-out optional negotiation consumed one of
    /// the bounded pending slots until the process eventually replied or died.
    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        timeout_message: &str,
    ) -> Result<Value, SessionError> {
        self.request_with_timeout_and_size(method, params, timeout, timeout_message)
            .await
            .map(|(value, _)| value)
    }

    async fn request_with_timeout_and_size(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        timeout_message: &str,
    ) -> Result<(Value, usize), SessionError> {
        let (id, rx, encoded_bytes) = self.begin_request(method, params).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(value))) => Ok((value, encoded_bytes)),
            Ok(Ok(Err(error))) => Err(SessionError::Send(error.message)),
            Ok(Err(_)) => Err(SessionError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(SessionError::Send(timeout_message.to_string()))
            }
        }
    }

    async fn send_interject_request(
        &self,
        text: String,
        content: Option<Vec<ContentBlock>>,
    ) -> Result<usize, SessionError> {
        if !self.negotiated.interject {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::MidTurnSteer,
            ));
        }
        if !self.turn_active.load(Ordering::Acquire) {
            return Err(SessionError::Send(
                "Grok Build interject requires an active turn".to_string(),
            ));
        }
        let correlation = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut params = json!({
            "sessionId": self.session_id,
            "text": text,
            "interjectionId": format!("umadev-{}-{correlation}", std::process::id()),
        });
        if let Some(content) = content {
            params["content"] = serde_json::to_value(content)
                .map_err(|error| SessionError::Send(format!("encode Grok interject: {error}")))?;
        }
        let (response, encoded_bytes) = self
            .request_with_timeout_and_size(
                "_x.ai/interject",
                params,
                INTERJECT_RESPONSE_WAIT,
                "Grok Build interject acknowledgement timed out; delivery is unknown and was not retried",
            )
            .await?;
        let status = response
            .get("status")
            .or_else(|| response.pointer("/result/status"))
            .and_then(Value::as_str);
        if status != Some("queued") {
            return Err(SessionError::Send(
                "Grok Build interject did not return its queued acknowledgement".to_string(),
            ));
        }
        Ok(encoded_bytes)
    }

    async fn send_prompt_blocks(
        &mut self,
        prompt: Vec<ContentBlock>,
    ) -> Result<usize, SessionError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let prompt_id = grok_prompt_id(id);
        // Grok's published queue/streaming implementation consumes promptId
        // for turn correlation and clientIdentifier for ownership attribution.
        // Both live in vendor metadata, so standard ACP peers may ignore them.
        let request = PromptRequest::new(self.session_id.clone(), prompt);
        let request = if matches!(self.vendor, AcpVendor::Grok) {
            request.meta(grok_prompt_meta(self.permissions, id).as_object().cloned())
        } else {
            request
        };
        let params = serde_json::to_value(request)
            .map_err(|e| SessionError::Send(format!("encode ACP session/prompt: {e}")))?;
        let frame = json!({
            "jsonrpc":"2.0", "id":id, "method":"session/prompt", "params":params
        });
        let encoded_bytes = serde_json::to_vec(&frame)
            .map_err(|e| SessionError::Send(format!("encode ACP session/prompt frame: {e}")))?
            .len();
        if encoded_bytes > MAX_OUTBOUND_BYTES {
            // Structured input is charged to its exact block before this point.
            // Reaching this guard therefore means the server-controlled session
            // envelope (rather than an arbitrary "text block 0") exhausted the
            // remaining wire budget.
            return Err(SessionError::Send(format!(
                "encoded {} ACP prompt exceeds UmaDev's 64 MiB wire limit",
                self.vendor.display_name()
            )));
        }
        if self.turn_active.swap(true, Ordering::AcqRel) {
            return Err(SessionError::Send(
                "an ACP turn is already in progress".to_string(),
            ));
        }
        if let Ok(mut active_prompt) = self.active_prompt.write() {
            *active_prompt = Some(ActivePromptRecord {
                prompt_id: prompt_id.clone(),
                request_id: id,
            });
        } else {
            self.turn_active.store(false, Ordering::Release);
            return Err(SessionError::Send(
                "ACP active-prompt state is unavailable".to_string(),
            ));
        }
        self.session_routes.lock().await.begin_turn(&prompt_id);
        self.latest_usage.lock().await.take();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= MAX_PENDING_REQUESTS {
                self.turn_active.store(false, Ordering::Release);
                clear_active_prompt(&self.active_prompt, &prompt_id);
                return Err(SessionError::Send(
                    "ACP pending-request limit reached".to_string(),
                ));
            }
            pending.insert(id, tx);
        }
        if let Err(error) = write_json_line(&self.writer, &frame).await {
            self.pending.lock().await.remove(&id);
            self.turn_active.store(false, Ordering::Release);
            clear_active_prompt(&self.active_prompt, &prompt_id);
            return Err(error);
        }
        self.spawn_prompt_completion(prompt_id, rx);
        Ok(encoded_bytes)
    }

    fn spawn_prompt_completion(&self, prompt_id: String, rx: PendingReceiver) {
        let event_tx = self.event_tx.clone();
        let active = Arc::clone(&self.turn_active);
        let active_prompt = Arc::clone(&self.active_prompt);
        let latest_usage = Arc::clone(&self.latest_usage);
        let session_routes = Arc::clone(&self.session_routes);
        let queued_prompts = Arc::clone(&self.queued_prompts);
        let pending_running_prompt = Arc::clone(&self.pending_running_prompt);
        tokio::spawn(async move {
            let (status, inline_usage) = match rx.await {
                Ok(Ok(result)) => parse_prompt_result(&result),
                Ok(Err(error)) => (
                    TurnStatus::Failed(error.message),
                    error.prompt_usage.map(|usage| *usage),
                ),
                Err(_) => (
                    TurnStatus::Failed("ACP process closed during prompt".to_string()),
                    None,
                ),
            };
            let streamed_usage = latest_usage.lock().await.take();
            let usage = inline_usage.or(streamed_usage);
            if take_active_prompt(&active_prompt, &prompt_id).is_some() {
                let terminal = session_routes
                    .lock()
                    .await
                    .settle_root(&prompt_id, status, usage);
                emit_converged_terminal(&event_tx, &active, terminal).await;
                promote_pending_queued_prompt(
                    &queued_prompts,
                    &pending_running_prompt,
                    &active_prompt,
                    active.as_ref(),
                    &session_routes,
                    &latest_usage,
                )
                .await;
            }
        });
    }

    async fn enqueue_prompt_blocks(
        &mut self,
        prompt: Vec<ContentBlock>,
        placement: PromptQueuePlacement,
    ) -> Result<usize, SessionError> {
        if !self.negotiated.prompt_queue {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::PromptQueue,
            ));
        }
        if !self.turn_active.load(Ordering::Acquire) {
            return Err(SessionError::Send(
                "Grok Build can queue a prompt only while a turn is active".to_string(),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let prompt_id = grok_prompt_id(id);
        let queue_meta = grok_queue_prompt_meta(
            &prompt_id,
            matches!(placement, PromptQueuePlacement::SendNow),
        )
        .map_err(|error| SessionError::Send(format!("encode Grok queue metadata: {error}")))?;
        let mut meta = grok_prompt_meta(self.permissions, id);
        if let (Some(target), Some(queue_fields)) = (meta.as_object_mut(), queue_meta.as_object()) {
            target.extend(queue_fields.clone());
        }
        let request =
            PromptRequest::new(self.session_id.clone(), prompt).meta(meta.as_object().cloned());
        let params = serde_json::to_value(request)
            .map_err(|error| SessionError::Send(format!("encode ACP queued prompt: {error}")))?;
        let frame = json!({
            "jsonrpc":"2.0", "id":id, "method":"session/prompt", "params":params
        });
        let encoded_bytes = serde_json::to_vec(&frame)
            .map_err(|error| {
                SessionError::Send(format!("encode ACP queued prompt frame: {error}"))
            })?
            .len();
        if encoded_bytes > MAX_OUTBOUND_BYTES {
            return Err(SessionError::Send(
                "encoded Grok Build queued prompt exceeds the 64 MiB wire limit".to_string(),
            ));
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.len() >= MAX_PENDING_REQUESTS {
                return Err(SessionError::Send(
                    "ACP pending-request limit reached".to_string(),
                ));
            }
            pending.insert(id, tx);
        }
        self.queued_prompts
            .lock()
            .await
            .insert(prompt_id.clone(), id);
        if let Err(error) = write_json_line(&self.writer, &frame).await {
            self.pending.lock().await.remove(&id);
            self.queued_prompts.lock().await.remove(&prompt_id);
            return Err(error);
        }

        self.spawn_queued_prompt_completion(prompt_id, rx);
        Ok(encoded_bytes)
    }

    fn spawn_queued_prompt_completion(&self, prompt_id: String, rx: PendingReceiver) {
        let event_tx = self.event_tx.clone();
        let active = Arc::clone(&self.turn_active);
        let active_prompt = Arc::clone(&self.active_prompt);
        let latest_usage = Arc::clone(&self.latest_usage);
        let session_routes = Arc::clone(&self.session_routes);
        let queued_prompts = Arc::clone(&self.queued_prompts);
        let pending_running_prompt = Arc::clone(&self.pending_running_prompt);
        tokio::spawn(async move {
            let (status, inline_usage) = match rx.await {
                Ok(Ok(result)) => parse_prompt_result(&result),
                Ok(Err(error)) => (
                    TurnStatus::Failed(error.message),
                    error.prompt_usage.map(|usage| *usage),
                ),
                Err(_) => (
                    TurnStatus::Failed("ACP process closed during queued prompt".to_string()),
                    None,
                ),
            };
            queued_prompts.lock().await.remove(&prompt_id);
            let streamed_usage = latest_usage.lock().await.take();
            let usage = inline_usage.or(streamed_usage);
            if take_active_prompt(&active_prompt, &prompt_id).is_some() {
                let terminal = session_routes
                    .lock()
                    .await
                    .settle_root(&prompt_id, status, usage);
                emit_converged_terminal(&event_tx, &active, terminal).await;
                promote_pending_queued_prompt(
                    &queued_prompts,
                    &pending_running_prompt,
                    &active_prompt,
                    active.as_ref(),
                    &session_routes,
                    &latest_usage,
                )
                .await;
            } else {
                let mut pending = pending_running_prompt.lock().await;
                if pending.as_deref() == Some(prompt_id.as_str()) {
                    pending.take();
                }
            }
        });
    }
}

fn acp_content_blocks(
    input: &crate::turn_input::PreparedTurnInput,
    capabilities: NegotiatedCapabilities,
    vendor: AcpVendor,
) -> Result<(Vec<ContentBlock>, Vec<InputDelivery>), SessionError> {
    let mut prompt = Vec::with_capacity(input.blocks.len());
    let mut deliveries = Vec::with_capacity(input.blocks.len());
    for (index, block) in input.blocks.iter().enumerate() {
        match block {
            crate::turn_input::PreparedBlock::Text(text) => {
                push_input_prompt_block(
                    &mut prompt,
                    ContentBlock::from(text.clone()),
                    index,
                    TurnInputBlockKind::Text,
                )?;
                deliveries.push(InputDelivery::Native);
            }
            crate::turn_input::PreparedBlock::Image(attachment) => {
                if !capabilities.image {
                    return Err(crate::turn_input::unsupported(
                        index,
                        TurnInputBlockKind::Image,
                        "the selected ACP base did not negotiate image prompt capability",
                    ));
                }
                push_input_prompt_block(
                    &mut prompt,
                    ContentBlock::Image(ImageContent::new(
                        base64::engine::general_purpose::STANDARD.encode(&attachment.bytes),
                        attachment.media_type.clone(),
                    )),
                    index,
                    TurnInputBlockKind::Image,
                )?;
                deliveries.push(InputDelivery::Native);
            }
            crate::turn_input::PreparedBlock::File { attachment, .. } => {
                if !capabilities.embedded_context {
                    return Err(crate::turn_input::unsupported(
                        index,
                        TurnInputBlockKind::File,
                        "the selected ACP base did not negotiate embedded-context capability",
                    ));
                }
                let uri = crate::turn_input::file_uri(
                    &attachment.canonical_path,
                    index,
                    TurnInputBlockKind::File,
                )?;
                let resource = if matches!(vendor, AcpVendor::Kimi) {
                    if attachment.bytes.contains(&0)
                        || std::str::from_utf8(&attachment.bytes).is_err()
                    {
                        return Err(crate::turn_input::unsupported(
                            index,
                            TurnInputBlockKind::File,
                            "Kimi Code ACP accepts only UTF-8 text embedded resources; its official adapter drops binary blob resources",
                        ));
                    }
                    EmbeddedResourceResource::TextResourceContents(
                        TextResourceContents::new(
                            std::str::from_utf8(&attachment.bytes)
                                .expect("Kimi embedded text was validated as UTF-8"),
                            uri,
                        )
                        .mime_type(attachment.media_type.clone()),
                    )
                } else if attachment.bytes.contains(&0) {
                    embedded_blob(attachment, uri)
                } else if let Ok(text) = std::str::from_utf8(&attachment.bytes) {
                    EmbeddedResourceResource::TextResourceContents(
                        TextResourceContents::new(text, uri)
                            .mime_type(attachment.media_type.clone()),
                    )
                } else {
                    embedded_blob(attachment, uri)
                };
                push_input_prompt_block(
                    &mut prompt,
                    ContentBlock::Resource(EmbeddedResource::new(resource)),
                    index,
                    TurnInputBlockKind::File,
                )?;
                deliveries.push(InputDelivery::Native);
            }
        }
    }
    Ok((prompt, deliveries))
}

fn encoded_prompt_content_bytes(prompt: &[ContentBlock]) -> Result<usize, SessionError> {
    serde_json::to_vec(prompt)
        .map(|bytes| bytes.len())
        .map_err(|error| SessionError::Send(format!("encode ACP prompt content: {error}")))
}

fn push_input_prompt_block(
    prompt: &mut Vec<ContentBlock>,
    block: ContentBlock,
    index: usize,
    kind: TurnInputBlockKind,
) -> Result<(), SessionError> {
    prompt.push(block);
    if encoded_prompt_content_bytes(prompt)? > MAX_PROMPT_CONTENT_BYTES {
        prompt.pop();
        return Err(SessionError::InputInvalid {
            index,
            kind,
            reason: "encoded input exceeds UmaDev's 64 MiB ACP safety limit".to_string(),
        });
    }
    Ok(())
}

fn embedded_blob(
    attachment: &crate::turn_input::PreparedAttachment,
    uri: String,
) -> EmbeddedResourceResource {
    EmbeddedResourceResource::BlobResourceContents(
        BlobResourceContents::new(
            base64::engine::general_purpose::STANDARD.encode(&attachment.bytes),
            uri,
        )
        .mime_type(attachment.media_type.clone()),
    )
}

type GrokInterjectOutput = (String, Option<Vec<ContentBlock>>, Vec<InputDelivery>);

fn grok_interject_blocks(
    input: &crate::turn_input::PreparedTurnInput,
    capabilities: NegotiatedCapabilities,
) -> Result<GrokInterjectOutput, SessionError> {
    let mut content = Vec::with_capacity(input.blocks.len());
    let mut deliveries = Vec::with_capacity(input.blocks.len());
    let mut raw_text = None;
    let mut has_image = false;
    for (index, block) in input.blocks.iter().enumerate() {
        match block {
            crate::turn_input::PreparedBlock::Text(text) => {
                if raw_text.is_some() {
                    return Err(crate::turn_input::unsupported(
                        index,
                        TurnInputBlockKind::Text,
                        "Grok Build interject accepts one model-text block",
                    ));
                }
                raw_text = Some(text.clone());
                push_input_prompt_block(
                    &mut content,
                    ContentBlock::from(text.clone()),
                    index,
                    TurnInputBlockKind::Text,
                )?;
                deliveries.push(InputDelivery::Native);
            }
            crate::turn_input::PreparedBlock::Image(attachment) => {
                if !capabilities.image {
                    return Err(crate::turn_input::unsupported(
                        index,
                        TurnInputBlockKind::Image,
                        "Grok Build did not negotiate image-capable interject",
                    ));
                }
                has_image = true;
                push_input_prompt_block(
                    &mut content,
                    ContentBlock::Image(ImageContent::new(
                        base64::engine::general_purpose::STANDARD.encode(&attachment.bytes),
                        attachment.media_type.clone(),
                    )),
                    index,
                    TurnInputBlockKind::Image,
                )?;
                deliveries.push(InputDelivery::Native);
            }
            crate::turn_input::PreparedBlock::File { .. } => {
                return Err(crate::turn_input::unsupported(
                    index,
                    TurnInputBlockKind::File,
                    "Grok Build interject supports text and image blocks, not embedded resources",
                ));
            }
        }
    }
    Ok((
        raw_text.unwrap_or_default(),
        has_image.then_some(content),
        deliveries,
    ))
}

#[async_trait]
impl BaseSession for AcpSession {
    fn capabilities(&self) -> SessionCapabilities {
        SessionCapabilities {
            mid_turn_steer: self.negotiated.interject,
            set_model: self.negotiated.set_model,
            set_mode: self.negotiated.set_mode,
            set_thinking: self.negotiated.kimi_source_contract,
            text_input: InputDelivery::Native,
            image_input: if self.negotiated.image {
                InputDelivery::Native
            } else {
                InputDelivery::Unsupported
            },
            file_input: if self.negotiated.embedded_context {
                InputDelivery::Native
            } else {
                InputDelivery::Unsupported
            },
            steer: if self.negotiated.interject {
                SteerSemantics::SameTurnOrImmediateNext
            } else {
                SteerSemantics::Unsupported
            },
            resume: if self.negotiated.resume {
                ResumeCapability::AcpResume
            } else if self.negotiated.load {
                ResumeCapability::AcpLoad
            } else {
                ResumeCapability::Unsupported
            },
            subagents: if self.negotiated.grok_source_contract {
                SubagentVisibility::Lifecycle
            } else {
                SubagentVisibility::None
            },
            prompt_queue: if self.negotiated.prompt_queue {
                PromptQueueCapability::ServerAuthoritativeVersioned
            } else {
                PromptQueueCapability::Unsupported
            },
            background_process_control: if self.negotiated.background_process_control {
                BackgroundProcessControlCapability::ServerAuthoritativeOwned
            } else {
                BackgroundProcessControlCapability::Unsupported
            },
        }
    }

    async fn set_model(
        &mut self,
        model_id: String,
        reasoning_effort: Option<SessionReasoningEffort>,
    ) -> Result<(), SessionError> {
        if !self.negotiated.set_model {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::SetModel,
            ));
        }
        if !valid_session_state_id(&model_id) {
            return Err(SessionError::Send(
                "ACP model id must be non-empty and at most 256 characters".to_string(),
            ));
        }
        match self.vendor {
            AcpVendor::Grok => {
                let mut params = json!({
                    "sessionId": self.session_id,
                    "modelId": model_id,
                });
                if let Some(effort) = reasoning_effort {
                    params["_meta"] = json!({"reasoningEffort": effort.as_str()});
                }
                self.request_with_timeout(
                    "session/set_model",
                    params,
                    SESSION_STATE_RESPONSE_WAIT,
                    "ACP session/set_model timed out",
                )
                .await?;
            }
            AcpVendor::Kimi => {
                if reasoning_effort.is_some() {
                    return Err(SessionError::Send(
                        "Kimi Code ACP exposes a separate on/off thinking option, not a graded reasoning-effort value"
                            .to_string(),
                    ));
                }
                let response = self
                    .request_with_timeout(
                        "session/set_config_option",
                        json!({
                            "sessionId":self.session_id,
                            "configId":"model",
                            "value":model_id
                        }),
                        SESSION_STATE_RESPONSE_WAIT,
                        "Kimi Code model selection timed out",
                    )
                    .await?;
                if extract_session_model(&response).as_deref() != Some(model_id.as_str()) {
                    return Err(SessionError::Send(
                        "Kimi Code did not confirm the selected model".to_string(),
                    ));
                }
            }
        }
        // The pinned Grok response body is not a stable model authority. RPC
        // success plus the requested catalog id is; live model_changed remains
        // the state/event rail for every subscribed client.
        self.model = model_id;
        Ok(())
    }

    async fn set_mode(&mut self, mode: SessionMode) -> Result<(), SessionError> {
        if !self.negotiated.set_mode {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::SetMode,
            ));
        }
        match self.vendor {
            AcpVendor::Grok => {
                self.request_with_timeout(
                    "session/set_mode",
                    json!({"sessionId": self.session_id, "modeId": mode.as_str()}),
                    SESSION_STATE_RESPONSE_WAIT,
                    "ACP session/set_mode timed out",
                )
                .await?;
            }
            AcpVendor::Kimi => {
                // Kimi has no distinct ask mode. Map the runtime's ask-first
                // presentation state to its approval-preserving default mode.
                let mode_id = match mode {
                    SessionMode::Plan => "plan",
                    SessionMode::Default | SessionMode::Ask => "default",
                };
                let response = self
                    .request_with_timeout(
                        "session/set_config_option",
                        json!({
                            "sessionId":self.session_id,
                            "configId":"mode",
                            "value":mode_id
                        }),
                        SESSION_STATE_RESPONSE_WAIT,
                        "Kimi Code mode selection timed out",
                    )
                    .await?;
                if extract_config_option_current(&response, "mode") != Some(mode_id) {
                    return Err(SessionError::Send(
                        "Kimi Code did not confirm the selected mode".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    async fn set_thinking(&mut self, enabled: bool) -> Result<SessionStateUpdate, SessionError> {
        if !self.negotiated.kimi_source_contract || !matches!(self.vendor, AcpVendor::Kimi) {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::SetThinking,
            ));
        }
        let response = self
            .request_with_timeout(
                "session/set_config_option",
                json!({
                    "sessionId":self.session_id,
                    "configId":"thinking",
                    "value":if enabled { "on" } else { "off" }
                }),
                SESSION_STATE_RESPONSE_WAIT,
                "Kimi Code thinking selection timed out",
            )
            .await?;
        let state = parse_thinking_state(&response);
        let SessionStateUpdate::ThinkingChanged {
            enabled: confirmed, ..
        } = &state
        else {
            unreachable!("thinking state parser returned a different update kind")
        };
        if *confirmed != Some(enabled) {
            return Err(SessionError::Send(
                "Kimi Code did not confirm the requested thinking state; the current model may lock or omit this control"
                    .to_string(),
            ));
        }
        Ok(state)
    }

    async fn fork(&mut self) -> Result<Box<dyn BaseSession>, SessionError> {
        let fork = Self::start_with_program_and_append_system(
            self.vendor,
            &self.program,
            &self.workspace,
            &self.model,
            BasePermissionProfile::Plan,
            self.append_system.as_deref(),
        )
        .await?;
        Ok(Box::new(fork))
    }

    async fn send_turn(&mut self, directive: String) -> Result<(), SessionError> {
        self.send_prompt_blocks(vec![ContentBlock::from(directive)])
            .await
            .map(|_| ())
    }

    async fn send_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        crate::turn_input::ensure_supported(&input, self.capabilities())?;
        let prepared = crate::turn_input::prepare(input).await?;
        let (prompt, deliveries) = acp_content_blocks(&prepared, self.negotiated, self.vendor)?;
        let encoded_bytes = self.send_prompt_blocks(prompt).await?;
        Ok(prepared.report(&deliveries, encoded_bytes))
    }

    async fn steer(&mut self, directive: String) -> Result<(), SessionError> {
        self.send_interject_request(directive, None)
            .await
            .map(|_| ())
    }

    async fn steer_input(&mut self, input: TurnInput) -> Result<DeliveryReport, SessionError> {
        if !self.negotiated.interject {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::MidTurnSteer,
            ));
        }
        crate::turn_input::ensure_supported(&input, self.capabilities())?;
        let prepared = crate::turn_input::prepare(input).await?;
        let (text, content, deliveries) = grok_interject_blocks(&prepared, self.negotiated)?;
        let encoded_bytes = self.send_interject_request(text, content).await?;
        let mut report = prepared.report(&deliveries, encoded_bytes);
        report.receipt = DeliveryReceiptStage::ProtocolAcknowledged;
        Ok(report)
    }

    async fn enqueue_input(
        &mut self,
        input: TurnInput,
        placement: PromptQueuePlacement,
    ) -> Result<DeliveryReport, SessionError> {
        crate::turn_input::ensure_supported(&input, self.capabilities())?;
        let prepared = crate::turn_input::prepare(input).await?;
        let (prompt, deliveries) = acp_content_blocks(&prepared, self.negotiated, self.vendor)?;
        let encoded_bytes = self.enqueue_prompt_blocks(prompt, placement).await?;
        Ok(prepared.report(&deliveries, encoded_bytes))
    }

    async fn mutate_prompt_queue(
        &mut self,
        mutation: PromptQueueMutation,
    ) -> Result<(), SessionError> {
        if !self.negotiated.prompt_queue {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::PromptQueue,
            ));
        }
        let mutation = match mutation {
            PromptQueueMutation::Remove {
                id,
                expected_version,
            } => GrokQueueMutation::Remove {
                id,
                expected_version,
            },
            PromptQueueMutation::Reorder { ordered_ids } => {
                GrokQueueMutation::Reorder { ordered_ids }
            }
            PromptQueueMutation::Clear => GrokQueueMutation::Clear,
            PromptQueueMutation::Edit { id, new_text } => GrokQueueMutation::Edit { id, new_text },
            PromptQueueMutation::Interject {
                id,
                expected_version,
                new_text,
            } => GrokQueueMutation::Interject {
                id,
                expected_version,
                new_text,
            },
        };
        let (method, params) = mutation
            .encode(&self.session_id)
            .map_err(|error| SessionError::Send(format!("encode Grok queue mutation: {error}")))?;
        let frame = json!({
            "jsonrpc":"2.0",
            "method":format!("_{method}"),
            "params":params
        });
        write_json_line(&self.writer, &frame).await
    }

    async fn list_background_processes(
        &mut self,
    ) -> Result<BackgroundProcessSnapshot, SessionError> {
        self.fetch_grok_background_processes().await
    }

    async fn stop_background_process(
        &mut self,
        task_id: &str,
    ) -> Result<BackgroundProcessStopOutcome, SessionError> {
        if !self.negotiated.background_process_control {
            return Err(SessionError::CapabilityUnsupported(
                SessionCapability::BackgroundProcessControl,
            ));
        }
        if !valid_background_id(task_id) {
            return Err(SessionError::Send(
                "Grok Build background task id is invalid".to_string(),
            ));
        }

        // Grok's pinned terminal backend is shared by parent and child
        // sessions, while x.ai/task/kill itself accepts any task id. A fresh
        // server list is therefore the authority-bearing ownership check; a
        // cached lifecycle edge never authorizes this destructive operation.
        let before = self.fetch_grok_background_processes().await?;
        let Some(process) = before
            .processes
            .iter()
            .find(|process| process.task_id == task_id)
        else {
            return Ok(BackgroundProcessStopOutcome::NotFound);
        };
        if process.completed {
            return Ok(BackgroundProcessStopOutcome::AlreadyExited);
        }

        let response = self
            .request_with_timeout(
                GROK_TASK_KILL_METHOD,
                grok_task_kill_params(&self.session_id, task_id),
                GROK_BACKGROUND_CONTROL_WAIT,
                "Grok Build background-process stop timed out; outcome is unknown",
            )
            .await?;
        let outcome = parse_grok_task_kill_response(&response, task_id)
            .map_err(|message| SessionError::Send(message.to_string()))?;

        // The native result is exact and idempotent, but the list remains the
        // state authority. Best-effort refresh removes a stale live edge without
        // turning a successful stop into an error if the child closes first.
        let _ = self.fetch_grok_background_processes().await;
        Ok(outcome)
    }

    async fn next_event(&mut self) -> Option<SessionEvent> {
        if let Some(event) = self.deferred_events.pop_front() {
            Some(event)
        } else {
            self.events.recv().await
        }
    }

    async fn respond(
        &mut self,
        req_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        self.interaction_sessions.lock().await.remove(req_id);
        let Some(pending) = self.approvals.lock().await.remove(req_id) else {
            return Ok(());
        };
        let PendingHostRequest::Approval(record) = pending else {
            return reject_mismatched_host_response(&self.writer, pending).await;
        };
        let option = record.option_for_decision(decision);
        write_permission_response(&self.writer, &record.raw_id, option.as_deref(), None).await
    }

    async fn respond_host(
        &mut self,
        req_id: &str,
        response: HostResponse,
    ) -> Result<(), SessionError> {
        self.interaction_sessions.lock().await.remove(req_id);
        let Some(pending) = self.approvals.lock().await.remove(req_id) else {
            return Ok(());
        };
        write_host_response(&self.writer, pending, response).await
    }

    async fn interrupt(&mut self) -> Result<(), SessionError> {
        let approvals = {
            let mut map = self.approvals.lock().await;
            map.drain().map(|(_, pending)| pending).collect::<Vec<_>>()
        };
        self.interaction_sessions.lock().await.clear();
        let cancelled_subagents = self.session_routes.lock().await.cancel_descendants();
        if !cancelled_subagents.is_empty() {
            self.deferred_events.push_back(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Live {
                    agent_ids: Vec::new(),
                },
            ));
        }
        let turn_was_active = self.turn_active.load(Ordering::Acquire);
        if !turn_was_active && cancelled_subagents.is_empty() {
            for pending in approvals {
                let _ = write_cancelled_host_response(&self.writer, pending, None).await;
            }
            return Ok(());
        }
        let frame = json!({
            "jsonrpc":"2.0", "method":"session/cancel",
            "params":{"sessionId":self.session_id,"_meta":{"cancelSubagents":true}}
        });
        match tokio::time::timeout(CONTROL_WRITE_WAIT, write_json_line(&self.writer, &frame)).await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(SessionError::InterruptPending(
                    "the ACP control pipe stopped accepting session/cancel".to_string(),
                ));
            }
        }

        let _ = tokio::time::timeout(CONTROL_WRITE_WAIT, async {
            for pending in approvals {
                let _ = write_cancelled_host_response(&self.writer, pending, None).await;
            }
        })
        .await;
        let buffered_terminal = self
            .session_routes
            .lock()
            .await
            .settle_after_lifecycle()
            .map(|mut terminal| {
                terminal.status = TurnStatus::Interrupted;
                terminal
            });
        emit_converged_terminal(&self.event_tx, self.turn_active.as_ref(), buffered_terminal).await;
        promote_pending_queued_prompt(
            &self.queued_prompts,
            &self.pending_running_prompt,
            &self.active_prompt,
            self.turn_active.as_ref(),
            &self.session_routes,
            &self.latest_usage,
        )
        .await;
        let deadline = tokio::time::Instant::now() + INTERRUPT_RESPONSE_WAIT;
        while self.turn_active.load(Ordering::Acquire) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.turn_active.load(Ordering::Acquire) {
            Err(SessionError::InterruptPending(
                "the ACP base did not confirm a terminal turn after session/cancel".to_string(),
            ))
        } else {
            Ok(())
        }
    }

    async fn end(&mut self) -> Result<(), SessionError> {
        let exited = tokio::time::timeout(ACP_GRACEFUL_END_BUDGET, graceful_end(self))
            .await
            .unwrap_or(false);
        #[cfg(windows)]
        if let Some(job) = self.process_job.take() {
            if !exited {
                job.terminate();
            }
            drop(job);
        }
        if !exited {
            reap_isolated_process_tree(&self.child, END_REAP_BUDGET).await;
        }
        self.stderr_drain.shutdown().await;
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        self.stderr.snapshot()
    }

    fn try_exit_status(&self) -> Option<std::process::ExitStatus> {
        self.child.try_lock().ok()?.try_wait().ok().flatten()
    }

    fn session_id(&self) -> Option<&str> {
        (!self.session_id.is_empty()).then_some(self.session_id.as_str())
    }
}

async fn graceful_end(session: &mut AcpSession) -> bool {
    let _ = session.interrupt().await;
    if session.negotiated.grok_source_contract && !session.session_id.is_empty() {
        // The audited Grok source exposes terminal session shutdown through
        // this private extension rather than the optional ACP close flag.
        let _ = tokio::time::timeout(
            CONTROL_WRITE_WAIT + CLOSE_RESPONSE_WAIT,
            session.request_with_timeout(
                "_x.ai/session/close",
                json!({"sessionId": session.session_id.clone()}),
                CLOSE_RESPONSE_WAIT,
                "Grok session close timed out",
            ),
        )
        .await;
    } else if session.negotiated.close && !session.session_id.is_empty() {
        let request = CloseSessionRequest::new(session.session_id.clone());
        if let Ok(params) = serde_json::to_value(request) {
            let _ = tokio::time::timeout(
                CONTROL_WRITE_WAIT + CLOSE_RESPONSE_WAIT,
                session.request_with_timeout(
                    "session/close",
                    params,
                    CLOSE_RESPONSE_WAIT,
                    "ACP session/close timed out",
                ),
            )
            .await;
        }
    }
    let _ = tokio::time::timeout(CONTROL_WRITE_WAIT, close_acp_stdin(&session.writer)).await;
    let graceful_budget = if session.negotiated.grok_source_contract {
        GROK_EOF_GRACE
    } else {
        END_REAP_BUDGET
    };
    wait_for_child_exit(&session.child, graceful_budget).await
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        #[cfg(windows)]
        if let Some(job) = self.process_job.take() {
            job.terminate();
            drop(job);
        }
        if let Ok(mut child) = self.child.try_lock() {
            kill_isolated_process_tree(&mut child);
        }
    }
}

fn pending_response(
    result: Result<Result<Value, AcpResponseError>, oneshot::error::RecvError>,
) -> Result<Value, SessionError> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(SessionError::Start(format!(
            "Grok authentication failed: {}",
            error.message
        ))),
        Err(_) => Err(SessionError::Closed),
    }
}

fn grok_auth_attempt_error(error: GrokAuthAttemptError) -> SessionError {
    SessionError::Start(format!(
        "Grok authentication policy rejected the operation: {error}"
    ))
}

fn grok_auth_stop_error(reason: GrokAuthStopReason) -> SessionError {
    let message = match reason {
        GrokAuthStopReason::Cancelled => "Grok authentication was cancelled",
        GrokAuthStopReason::TimedOut => "Grok authentication timed out",
        GrokAuthStopReason::InitializeContract(_) => {
            "Grok initialize returned an invalid authentication catalog"
        }
        GrokAuthStopReason::AuthUrlContract(_) => {
            "Grok returned an invalid authentication challenge"
        }
        GrokAuthStopReason::AuthUrlPollExhausted => {
            "Grok did not publish an authentication challenge"
        }
        GrokAuthStopReason::AuthenticationFailed => "Grok authentication failed",
        GrokAuthStopReason::TransportClosed => {
            "Grok closed its protocol transport during authentication"
        }
        GrokAuthStopReason::EventReceiverClosed => {
            "the Grok authentication interaction surface closed"
        }
    };
    SessionError::Start(message.to_string())
}

fn legacy_session_open_error(error: SessionOpenError) -> SessionError {
    match error {
        SessionOpenError::Session(error) => error,
        SessionOpenError::AuthRequired(_) => SessionError::Start(
            "Grok Build authentication requires explicit user confirmation; use the policy-aware session opener"
                .to_string(),
        ),
    }
}

async fn close_acp_stdin(writer: &SharedWriter) {
    let stdin = writer.lock().await.take();
    if let Some(mut stdin) = stdin {
        let _ = stdin.shutdown().await;
    }
}

async fn wait_for_child_exit(
    child: &std::sync::Mutex<tokio::process::Child>,
    budget: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if matches!(
            child.try_lock().map(|mut child| child.try_wait()),
            Ok(Ok(Some(_)))
        ) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[derive(Debug)]
struct ApprovalRecord {
    raw_id: Value,
    options: Vec<HostApprovalOption>,
}

impl ApprovalRecord {
    fn option_for_decision(&self, decision: ApprovalDecision) -> Option<String> {
        // A binary policy response is scoped to this request. Persistent
        // options are valid only when a richer UI returns their exact id via
        // `selected_option_id`; absence of a one-shot choice means cancelled.
        let kind = match decision {
            ApprovalDecision::Allow => HostApprovalOptionKind::AllowOnce,
            ApprovalDecision::Deny => HostApprovalOptionKind::RejectOnce,
        };
        self.options
            .iter()
            .find(|option| option.kind == kind)
            .map(|option| option.id.clone())
    }

    fn validates_selected(&self, id: &str, decision: ApprovalDecision) -> bool {
        self.options.iter().any(|option| {
            option.id == id
                && matches!(
                    (decision, &option.kind),
                    (
                        ApprovalDecision::Allow,
                        HostApprovalOptionKind::AllowOnce | HostApprovalOptionKind::AllowAlways
                    ) | (
                        ApprovalDecision::Deny,
                        HostApprovalOptionKind::RejectOnce | HostApprovalOptionKind::RejectAlways
                    )
                )
        })
    }
}

#[derive(Debug)]
enum PendingHostRequest {
    Approval(ApprovalRecord),
    UserInput {
        raw_id: Value,
        questions: Vec<PendingQuestion>,
        flavor: UserInputFlavor,
    },
    PermissionExpansion {
        raw_id: Value,
    },
    McpElicitation {
        raw_id: Value,
    },
    PlanConfirmation {
        raw_id: Value,
        flavor: PlanConfirmationFlavor,
    },
    FolderTrust {
        raw_id: Value,
        /// Per-request identity used by the detached deadline task. JSON-RPC
        /// ids may be reused after settlement, so an old timer must not reject
        /// a newer request that happens to carry the same wire id.
        timeout_token: Arc<()>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserInputFlavor {
    Generic,
    GrokAskUserQuestion,
    /// Kimi's source-audited AskUserQuestion bridge reuses
    /// `session/request_permission` with q0_opt_* option ids.
    KimiPermissionQuestion,
    /// Kimi's source-audited plan review also travels over
    /// `session/request_permission`, but every `plan_*` option is a distinct
    /// product decision rather than a binary allow/deny alias.
    KimiPlanReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanConfirmationFlavor {
    Generic,
    GrokExitPlanMode,
}

#[derive(Debug)]
struct PendingQuestion {
    id: String,
    prompt: String,
    option_labels: HashMap<String, String>,
    option_previews: HashMap<String, String>,
    multi_select: bool,
}

impl PendingHostRequest {
    fn raw_id(&self) -> &Value {
        match self {
            Self::Approval(record) => &record.raw_id,
            Self::UserInput { raw_id, .. }
            | Self::PermissionExpansion { raw_id }
            | Self::McpElicitation { raw_id }
            | Self::PlanConfirmation { raw_id, .. }
            | Self::FolderTrust { raw_id, .. } => raw_id,
        }
    }
}

struct ReaderContext {
    vendor: AcpVendor,
    writer: SharedWriter,
    pending: PendingMap,
    approvals: ApprovalMap,
    turn_active: Arc<AtomicBool>,
    active_session_id: ActiveSessionId,
    active_prompt: ActivePrompt,
    latest_usage: LatestUsage,
    event_tx: mpsc::Sender<SessionEvent>,
    permissions: BasePermissionProfile,
    grok_source_contract: Arc<AtomicBool>,
    handshake_in_progress: Arc<AtomicBool>,
    fresh_session_bind_in_progress: Arc<AtomicBool>,
    session_routes: SessionRoutes,
    interaction_sessions: InteractionSessions,
    replay_session_state: ReplaySessionState,
    background_processes: BackgroundProcesses,
    prompt_queue: PromptQueueMirror,
    queued_prompts: QueuedPrompts,
    pending_running_prompt: PendingRunningPrompt,
    folder_trust_scope: FolderTrustScopeState,
    deferred_folder_trust_requests: DeferredFolderTrustRequests,
    folder_trust_surface: FolderTrustClientSurface,
}

async fn reader_loop(stdout: tokio::process::ChildStdout, context: ReaderContext) {
    let mut reader = BufReader::new(stdout);
    let mut tools = ToolState::default();
    let mut terminal_error = "ACP process closed".to_string();
    loop {
        match read_bounded_frame(&mut reader).await {
            Ok(Some(FrameRead::Line(line))) => {
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                dispatch_frame(frame, &context, &mut tools).await;
            }
            Ok(Some(FrameRead::Oversized)) => {
                terminal_error = "ACP frame exceeded the 64 MiB safety limit".to_string();
                if context.turn_active.swap(false, Ordering::AcqRel) {
                    let _ = context
                        .event_tx
                        .send(SessionEvent::TurnDone {
                            status: TurnStatus::Failed(terminal_error.clone()),
                            usage: None,
                        })
                        .await;
                }
                break;
            }
            Ok(None) => break,
            Err(error) => {
                terminal_error = if error.kind() == std::io::ErrorKind::InvalidData {
                    "ACP emitted invalid UTF-8".to_string()
                } else {
                    "ACP stdout read failed".to_string()
                };
                break;
            }
        }
    }

    if let Ok(mut active_prompt) = context.active_prompt.write() {
        active_prompt.take();
    }

    let mut pending = context.pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(AcpResponseError::message(terminal_error.clone())));
    }
    drop(pending);
    context.interaction_sessions.lock().await.clear();
    context.session_routes.lock().await.cancel_descendants();
    if context.turn_active.swap(false, Ordering::AcqRel) {
        let _ = context
            .event_tx
            .send(SessionEvent::TurnDone {
                status: TurnStatus::Failed(terminal_error),
                usage: None,
            })
            .await;
    }
}

/// One server message after applying ACP's private-extension wire convention.
/// Standard methods are returned byte-for-byte; only `_x.ai/*` may lose the
/// transport underscore or unwrap the leader's nested method/params envelope.
struct NormalizedServerMessage<'a> {
    method: &'a str,
    params: &'a Value,
}

fn normalize_server_message(frame: &Value) -> Result<NormalizedServerMessage<'_>, &'static str> {
    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .ok_or("ACP server message omitted method")?;
    let params = frame.get("params").unwrap_or(&Value::Null);
    let Some(logical_method) = method.strip_prefix('_') else {
        return Ok(NormalizedServerMessage { method, params });
    };
    if !logical_method.starts_with("x.ai/") {
        // Never strip a leading underscore from standard or unknown methods.
        return Ok(NormalizedServerMessage { method, params });
    }

    let Some(inner_method_value) = params.get("method") else {
        return Ok(NormalizedServerMessage {
            method: logical_method,
            params,
        });
    };
    let inner_method = inner_method_value
        .as_str()
        .ok_or("Grok leader extension wrapper used a non-string method")?;
    let inner_logical = inner_method.strip_prefix('_').unwrap_or(inner_method);
    if inner_logical != logical_method || !inner_logical.starts_with("x.ai/") {
        return Err("Grok leader extension wrapper method did not match its envelope");
    }
    let inner_params = params
        .get("params")
        .ok_or("Grok leader extension wrapper omitted inner params")?;
    Ok(NormalizedServerMessage {
        method: inner_logical,
        params: inner_params,
    })
}

async fn dispatch_frame(frame: Value, context: &ReaderContext, tools: &mut ToolState) {
    if dispatch_rpc_response(&frame, context).await {
        return;
    }
    let has_id = frame.get("id").is_some();
    if !jsonrpc_version_is_compatible(&frame) {
        if has_id {
            reply_rpc_error(
                &context.writer,
                frame.get("id").cloned().unwrap_or(Value::Null),
                -32_600,
                "ACP requires JSON-RPC 2.0",
            )
            .await;
        }
        return;
    }
    let normalized = match normalize_server_message(&frame) {
        Ok(normalized) => normalized,
        Err(message) => {
            if has_id {
                reply_rpc_error(
                    &context.writer,
                    frame.get("id").cloned().unwrap_or(Value::Null),
                    -32_602,
                    message,
                )
                .await;
            }
            return;
        }
    };
    let method = normalized.method;
    let is_consumed_grok_notification = matches!(
        method,
        "x.ai/session_notification"
            | "x.ai/session/update"
            | "x.ai/models/update"
            | "x.ai/task_backgrounded"
            | "x.ai/task_completed"
            | QUEUE_CHANGED_METHOD
    );
    if !has_id && suppress_notification(method, &frame) && !is_consumed_grok_notification {
        return;
    }
    dispatch_server_message(&frame, method, normalized.params, has_id, context, tools).await;
}

async fn dispatch_rpc_response(frame: &Value, context: &ReaderContext) -> bool {
    if frame.get("method").is_some() || frame.get("id").is_none() {
        return false;
    }
    // Every no-method frame with an id is response-shaped. A string/negative/
    // otherwise wrong id cannot match the u64 request ids we emitted, but it is
    // still not a server request and must not leak into the host-request UI.
    let Some(id) = frame.get("id").and_then(Value::as_u64) else {
        return true;
    };
    let sender = context.pending.lock().await.remove(&id);
    if let Some(sender) = sender {
        let _ = sender.send(acp_response_payload(frame));
    }
    true
}

fn acp_response_payload(frame: &Value) -> Result<Value, AcpResponseError> {
    if !jsonrpc_version_is_compatible(frame) {
        return Err(AcpResponseError::message(
            "ACP response used an incompatible JSON-RPC version",
        ));
    }
    match (frame.get("result"), frame.get("error")) {
        (Some(result), None) => Ok(sanitize_value(result.clone())),
        (None, Some(error)) => Err(AcpResponseError {
            message: safe_rpc_error(error),
            code: error.get("code").and_then(Value::as_i64),
            prompt_usage: error
                .pointer("/data/promptUsage")
                .and_then(parse_usage)
                .map(Box::new),
        }),
        (Some(_), Some(_)) => Err(AcpResponseError::message(
            "malformed ACP response contains both result and error",
        )),
        (None, None) => Err(AcpResponseError::message(
            "malformed ACP response contains neither result nor error",
        )),
    }
}

/// ACP is JSON-RPC 2.0. Missing `jsonrpc` remains tolerated for observed CLI
/// compatibility, but an explicitly different version must never be accepted
/// as a successful response or an authority-bearing server request.
fn jsonrpc_version_is_compatible(frame: &Value) -> bool {
    frame
        .get("jsonrpc")
        .is_none_or(|version| version.as_str() == Some("2.0"))
}

async fn dispatch_server_message(
    frame: &Value,
    method: &str,
    params: &Value,
    has_id: bool,
    context: &ReaderContext,
    tools: &mut ToolState,
) {
    let replaying = context.handshake_in_progress.load(Ordering::Acquire);
    // During a fresh session/new Grok can emit Folder Trust before returning
    // the authoritative session id. Admit only that exact reverse method into
    // the bounded deferred validator; every other pre-bind session message
    // still fails route authorization.
    let early_folder_trust = has_id
        && method == GROK_FOLDER_TRUST_REQUEST_METHOD
        && context.grok_source_contract.load(Ordering::Acquire)
        && matches!(
            context.folder_trust_surface,
            FolderTrustClientSurface::Interactive
        )
        && context
            .fresh_session_bind_in_progress
            .load(Ordering::Acquire);
    let session_matches = early_folder_trust
        || if context.grok_source_contract.load(Ordering::Acquire) {
            context
                .session_routes
                .lock()
                .await
                .authorizes(params, replaying)
        } else {
            acp_message_matches_active_session(params, &context.active_session_id)
        };
    if !session_matches {
        if has_id {
            reply_rpc_error(
                &context.writer,
                frame.get("id").cloned().unwrap_or(Value::Null),
                -32_602,
                "request belongs to a different ACP session",
            )
            .await;
        }
        return;
    }
    if has_id
        && context.grok_source_contract.load(Ordering::Acquire)
        && matches!(
            method,
            "session/request_permission" | "x.ai/ask_user_question" | "x.ai/exit_plan_mode"
        )
        && params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(Value::as_str)
            .is_none()
    {
        reply_rpc_error(
            &context.writer,
            frame.get("id").cloned().unwrap_or(Value::Null),
            -32_602,
            "Grok interaction request omitted its ACP session id",
        )
        .await;
        return;
    }
    match (method, has_id) {
        ("session/update", false) => {
            handle_session_update_message(params, context, tools, replaying).await;
        }
        ("x.ai/session_notification" | "x.ai/session/update", false) => {
            handle_grok_session_notification(method, params, context, tools, replaying).await;
        }
        ("x.ai/task_backgrounded" | "x.ai/task_completed", false) => {
            handle_grok_background_lifecycle(method, params, context, tools, replaying).await;
        }
        ("x.ai/models/update", false) => {
            handle_grok_model_update(params, context, replaying).await;
        }
        (QUEUE_CHANGED_METHOD, false) => {
            handle_grok_queue_changed(params, context, replaying).await;
        }
        (method, true) => {
            dispatch_host_request(frame, method, params, context, replaying).await;
        }
        _ => {}
    }
}

async fn dispatch_host_request(
    frame: &Value,
    method: &str,
    params: &Value,
    context: &ReaderContext,
    replaying: bool,
) {
    let grok_contract = context.grok_source_contract.load(Ordering::Acquire);
    match method {
        GROK_FOLDER_TRUST_REQUEST_METHOD => {
            handle_folder_trust_request(frame, params, context, replaying).await;
        }
        "session/request_permission" => {
            if context.vendor == AcpVendor::Kimi {
                match kimi_permission_surface(params) {
                    KimiPermissionSurface::Question => {
                        handle_kimi_permission_question(frame, params, context).await;
                        return;
                    }
                    KimiPermissionSurface::PlanReview => {
                        handle_kimi_plan_review_request(frame, params, context).await;
                        return;
                    }
                    KimiPermissionSurface::Ordinary => {}
                }
            }
            handle_permission_request(frame, params, context, grok_contract).await;
        }
        method
            if is_standard_ask_question_method(method)
                || (is_grok_ask_user_question_method(method) && grok_contract) =>
        {
            handle_user_input_request(
                frame,
                method,
                params,
                &context.writer,
                &context.approvals,
                &context.interaction_sessions,
                &context.event_tx,
            )
            .await;
        }
        method if is_permission_expansion_method(method) => {
            handle_permission_expansion_request(
                frame,
                params,
                &context.writer,
                &context.approvals,
                &context.interaction_sessions,
                &context.event_tx,
            )
            .await;
        }
        "elicitation/create" => {
            handle_elicitation_request(
                frame,
                params,
                &context.writer,
                &context.approvals,
                &context.interaction_sessions,
                &context.event_tx,
            )
            .await;
        }
        method
            if is_standard_plan_confirmation_method(method)
                || (is_grok_exit_plan_mode_method(method) && grok_contract) =>
        {
            handle_plan_confirmation_request(
                frame,
                method,
                params,
                &context.writer,
                &context.approvals,
                &context.interaction_sessions,
                &context.event_tx,
            )
            .await;
        }
        method => handle_unknown_server_request(frame, method, params, context).await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KimiPermissionSurface {
    Ordinary,
    Question,
    PlanReview,
}

/// Classify Kimi's three source-defined uses of `session/request_permission`
/// before parsing their full payloads. A malformed `AskUserQuestion` or
/// `plan_*` request must stay on its human-input path and fail closed there;
/// falling through to ordinary tool approval could otherwise turn a question
/// into authority to execute an action.
fn kimi_permission_surface(params: &Value) -> KimiPermissionSurface {
    if params.pointer("/toolCall/title").and_then(Value::as_str) == Some("AskUserQuestion") {
        return KimiPermissionSurface::Question;
    }
    if params
        .get("options")
        .and_then(Value::as_array)
        .is_some_and(|options| {
            options.iter().any(|option| {
                option
                    .get("optionId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("plan_"))
            })
        })
    {
        return KimiPermissionSurface::PlanReview;
    }
    KimiPermissionSurface::Ordinary
}

async fn handle_session_update_message(
    params: &Value,
    context: &ReaderContext,
    tools: &mut ToolState,
    replaying: bool,
) {
    if replaying {
        let mut state = context.replay_session_state.lock().await;
        for event in parse_replay_session_state(params) {
            state.remember_event(event);
        }
        return;
    }
    if let Some(usage) = usage_from_event(params) {
        *context.latest_usage.lock().await = Some(usage);
    }
    for event in parse_session_update(params, tools) {
        if !matches!(event, SessionEvent::TurnDone { .. }) {
            emit_event(&context.event_tx, event).await;
        }
    }
}

async fn handle_grok_session_notification(
    method: &str,
    params: &Value,
    context: &ReaderContext,
    tools: &mut ToolState,
    replaying: bool,
) {
    if !context.grok_source_contract.load(Ordering::Acquire) {
        return;
    }
    if !replaying {
        if let Some(usage) = usage_from_event(params) {
            *context.latest_usage.lock().await = Some(usage);
        }
        if handle_grok_turn_completed(params, context).await {
            return;
        }
    }
    remember_or_emit_model_events(parse_grok_model_state_events(params), context, replaying).await;
    handle_grok_background_lifecycle(method, params, context, tools, replaying).await;
    handle_grok_subagent_lifecycle(params, context, replaying).await;
}

async fn handle_grok_background_lifecycle(
    method: &str,
    params: &Value,
    context: &ReaderContext,
    tools: &mut ToolState,
    replaying: bool,
) {
    if !context.grok_source_contract.load(Ordering::Acquire) {
        return;
    }
    let Some(update) = grok_background_process_lifecycle_event(method, params, tools, true) else {
        return;
    };
    let event = context
        .background_processes
        .lock()
        .await
        .apply(update, replaying);
    if let Some(event) = event {
        emit_event(&context.event_tx, event).await;
    }
}

async fn handle_grok_model_update(params: &Value, context: &ReaderContext, replaying: bool) {
    if !context.grok_source_contract.load(Ordering::Acquire) {
        return;
    }
    let events = parse_model_catalog(params)
        .map(SessionEvent::StateUpdate)
        .into_iter();
    remember_or_emit_model_events(events, context, replaying).await;
}

async fn remember_or_emit_model_events(
    events: impl IntoIterator<Item = SessionEvent>,
    context: &ReaderContext,
    replaying: bool,
) {
    if replaying {
        let mut state = context.replay_session_state.lock().await;
        for event in events {
            state.remember_event(event);
        }
    } else {
        for event in events {
            emit_event(&context.event_tx, event).await;
        }
    }
}

async fn handle_grok_queue_changed(params: &Value, context: &ReaderContext, replaying: bool) {
    if !context.grok_source_contract.load(Ordering::Acquire) {
        return;
    }
    let active_session_id = context.active_session_id.read().ok().and_then(|guard| {
        guard
            .as_deref()
            .filter(|session_id| !session_id.is_empty())
            .map(str::to_string)
    });
    let Some(active_session_id) = active_session_id else {
        return;
    };
    let Ok(snapshot) = GrokQueueSnapshot::parse(params, &active_session_id) else {
        return;
    };
    if context
        .prompt_queue
        .lock()
        .await
        .replace(snapshot.clone())
        .is_err()
    {
        return;
    }
    synchronize_running_queued_prompt(&snapshot, context).await;
    let event = SessionEvent::PromptQueueChanged(snapshot.into_runtime());
    remember_or_emit_model_events([event], context, replaying).await;
}

async fn synchronize_running_queued_prompt(snapshot: &GrokQueueSnapshot, context: &ReaderContext) {
    let running_prompt_id = snapshot.running_prompt_id().map(str::to_string);
    {
        let mut pending = context.pending_running_prompt.lock().await;
        if pending
            .as_deref()
            .is_some_and(|pending_id| Some(pending_id) != running_prompt_id.as_deref())
        {
            pending.take();
        }
    }
    if let Some(running_prompt_id) = running_prompt_id.as_deref() {
        let _ = promote_queued_prompt(
            running_prompt_id,
            &context.queued_prompts,
            &context.pending_running_prompt,
            &context.active_prompt,
            context.turn_active.as_ref(),
            &context.session_routes,
            &context.latest_usage,
        )
        .await;
    }
}

async fn handle_unknown_server_request(
    frame: &Value,
    method: &str,
    params: &Value,
    context: &ReaderContext,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    emit_event(
        &context.event_tx,
        SessionEvent::HostRequest {
            req_id: rpc_id_string(&raw_id),
            request: HostRequest::Unknown {
                method: method.to_string(),
                payload: sanitize_value(params.clone()),
            },
        },
    )
    .await;
    reply_rpc_error(
        &context.writer,
        raw_id,
        -32_601,
        "method not supported by UmaDev ACP client",
    )
    .await;
}

async fn handle_grok_subagent_lifecycle(params: &Value, context: &ReaderContext, replaying: bool) {
    let effect = context
        .session_routes
        .lock()
        .await
        .apply_lifecycle(params, replaying);
    let Ok(Some(effect)) = effect else {
        return;
    };
    match effect {
        LifecycleEffect::Started { subagent_id } => {
            if !replaying {
                emit_event(
                    &context.event_tx,
                    SessionEvent::BackgroundTask(BackgroundTaskSignal::Started { id: subagent_id }),
                )
                .await;
            }
        }
        LifecycleEffect::Finished {
            subagent_id,
            child_session_id,
            ..
        } => {
            if replaying {
                return;
            }
            cancel_interactions_for_session(context, &child_session_id).await;
            emit_event(
                &context.event_tx,
                SessionEvent::BackgroundTask(BackgroundTaskSignal::Finished { id: subagent_id }),
            )
            .await;
            let terminal = context.session_routes.lock().await.settle_after_lifecycle();
            emit_converged_terminal(&context.event_tx, context.turn_active.as_ref(), terminal)
                .await;
            promote_pending_queued_prompt(
                &context.queued_prompts,
                &context.pending_running_prompt,
                &context.active_prompt,
                context.turn_active.as_ref(),
                &context.session_routes,
                &context.latest_usage,
            )
            .await;
        }
        LifecycleEffect::Progress | LifecycleEffect::Duplicate => {}
    }
}

async fn cancel_interactions_for_session(context: &ReaderContext, session_id: &str) {
    let request_ids = {
        let mut sessions = context.interaction_sessions.lock().await;
        let ids = sessions
            .iter()
            .filter(|(_, owner)| *owner == session_id)
            .map(|(request_id, _)| request_id.clone())
            .collect::<Vec<_>>();
        for request_id in &ids {
            sessions.remove(request_id);
        }
        ids
    };
    for request_id in request_ids {
        let pending = context.approvals.lock().await.remove(&request_id);
        if let Some(pending) = pending {
            let _ = write_cancelled_host_response(&context.writer, pending, None).await;
        }
    }
}

async fn handle_grok_turn_completed(params: &Value, context: &ReaderContext) -> bool {
    let update = params.get("update").unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str);
    if kind != Some("turn_completed") {
        return false;
    }

    let Some(prompt_id) = update
        .get("prompt_id")
        .or_else(|| update.get("promptId"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };
    let Some(stop_reason) = update
        .get("stop_reason")
        .or_else(|| update.get("stopReason"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };
    let detail = update
        .get("agent_result")
        .or_else(|| update.get("agentResult"))
        .and_then(Value::as_str);
    let status = grok_turn_status(stop_reason, detail);
    if prompt_id.starts_with("subagent-completed-") {
        let streamed_usage = context.latest_usage.lock().await.take();
        let usage = usage_from_event(params).or(streamed_usage);
        let terminal = context
            .session_routes
            .lock()
            .await
            .settle_synthetic(prompt_id, status, usage);
        emit_converged_terminal(&context.event_tx, context.turn_active.as_ref(), terminal).await;
        promote_pending_queued_prompt(
            &context.queued_prompts,
            &context.pending_running_prompt,
            &context.active_prompt,
            context.turn_active.as_ref(),
            &context.session_routes,
            &context.latest_usage,
        )
        .await;
        return true;
    }

    let Some(active_prompt) = take_active_prompt(&context.active_prompt, prompt_id) else {
        // Replay and late terminals are idempotent and must never settle a newer
        // prompt merely because they share the same session.
        return true;
    };
    context
        .pending
        .lock()
        .await
        .remove(&active_prompt.request_id);
    context.queued_prompts.lock().await.remove(prompt_id);
    let streamed_usage = context.latest_usage.lock().await.take();
    let usage = usage_from_event(params).or(streamed_usage);
    let terminal = context
        .session_routes
        .lock()
        .await
        .settle_root(prompt_id, status, usage);
    emit_converged_terminal(&context.event_tx, context.turn_active.as_ref(), terminal).await;
    promote_pending_queued_prompt(
        &context.queued_prompts,
        &context.pending_running_prompt,
        &context.active_prompt,
        context.turn_active.as_ref(),
        &context.session_routes,
        &context.latest_usage,
    )
    .await;
    true
}

/// ACP session-scoped messages carry `params.sessionId`. Missing attribution is
/// tolerated for vendor extensions, but an explicitly different id is never
/// allowed to update the transcript or request authority from this session.
fn acp_message_matches_active_session(params: &Value, active: &ActiveSessionId) -> bool {
    let Some(reported) = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(Value::as_str)
    else {
        return true;
    };
    let Ok(expected) = active.read() else {
        return true;
    };
    expected
        .as_deref()
        .is_none_or(|expected| expected == reported)
}

/// Decode the exact question-over-permission shape used by the audited Kimi
/// ACP adapter. The strict title + q0 namespace prevents an ordinary approval
/// request with several allow choices from being reclassified as user input.
fn kimi_permission_question(params: &Value) -> Option<(HostQuestion, PendingQuestion)> {
    let tool = params.get("toolCall")?;
    if tool.get("title").and_then(Value::as_str) != Some("AskUserQuestion") {
        return None;
    }
    let prompt = tool.get("content")?.as_array()?.iter().find_map(|block| {
        block
            .pointer("/content/text")
            .or_else(|| block.get("text"))
            .and_then(Value::as_str)
    })?;
    if prompt.trim().is_empty() {
        return None;
    }
    let raw_options = params.get("options")?.as_array()?;
    if raw_options.is_empty() || raw_options.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }
    let answer_count = raw_options.len().checked_sub(1)?;
    let mut seen = HashSet::new();
    let mut options = Vec::with_capacity(raw_options.len());
    let mut option_labels = HashMap::with_capacity(raw_options.len());
    for (index, raw) in raw_options.iter().enumerate() {
        let id = raw.get("optionId").and_then(Value::as_str)?;
        let label = raw.get("name").and_then(Value::as_str)?;
        let expected_id = if index < answer_count {
            format!("q0_opt_{index}")
        } else {
            "q0_skip".to_string()
        };
        let expected_kind = if index < answer_count {
            "allow_once"
        } else {
            "reject_once"
        };
        if id != expected_id
            || raw.get("kind").and_then(Value::as_str) != Some(expected_kind)
            || (index == answer_count && label != "Skip")
            || !seen.insert(id)
            || label.trim().is_empty()
            || label.chars().count() > MAX_SESSION_STATE_TEXT_CHARS
        {
            return None;
        }
        options.push(HostQuestionOption {
            value: id.to_string(),
            label: clip_text(&redact_text(label), 240),
            description: None,
            preview: None,
        });
        option_labels.insert(id.to_string(), label.to_string());
    }
    let prompt = clip_text(&redact_text(prompt), 4_000);
    Some((
        HostQuestion {
            id: "q0".to_string(),
            header: Some("Kimi Code".to_string()),
            prompt: prompt.clone(),
            kind: HostQuestionKind::SingleChoice,
            required: false,
            options,
        },
        PendingQuestion {
            id: "q0".to_string(),
            prompt,
            option_labels,
            option_previews: HashMap::new(),
            multi_select: false,
        },
    ))
}

/// Decode the exact plan-review permission shape emitted by the pinned Kimi
/// ACP adapter. The option namespace and order are intentionally strict: an
/// ordinary tool approval with several allow choices must never acquire plan
/// semantics, and a future incompatible Kimi shape must fail closed instead of
/// silently selecting the first plan variant.
fn kimi_plan_review_permission(params: &Value) -> Option<(HostQuestion, PendingQuestion)> {
    let tool = params.get("toolCall")?;
    let raw_options = params.get("options")?.as_array()?;
    if raw_options.len() < 3 || raw_options.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }

    let approve_count = raw_options.len().checked_sub(2)?;
    let valid_approve_shape = if approve_count == 1 {
        raw_options[0].get("optionId").and_then(Value::as_str) == Some("plan_approve")
    } else {
        raw_options[..approve_count]
            .iter()
            .enumerate()
            .all(|(index, option)| {
                option.get("optionId").and_then(Value::as_str)
                    == Some(format!("plan_opt_{index}").as_str())
            })
    };
    if !valid_approve_shape
        || raw_options[approve_count]
            .get("optionId")
            .and_then(Value::as_str)
            != Some("plan_revise")
        || raw_options[approve_count + 1]
            .get("optionId")
            .and_then(Value::as_str)
            != Some("plan_reject_and_exit")
    {
        return None;
    }

    let mut seen = HashSet::new();
    let mut options = Vec::with_capacity(raw_options.len());
    let mut option_labels = HashMap::with_capacity(raw_options.len());
    for (index, raw) in raw_options.iter().enumerate() {
        let id = raw.get("optionId").and_then(Value::as_str)?;
        let label = raw.get("name").and_then(Value::as_str)?;
        let expected_kind = if index < approve_count {
            "allow_once"
        } else {
            "reject_once"
        };
        if raw.get("kind").and_then(Value::as_str) != Some(expected_kind)
            || !seen.insert(id)
            || label.trim().is_empty()
            || label.chars().count() > MAX_SESSION_STATE_TEXT_CHARS
        {
            return None;
        }
        options.push(HostQuestionOption {
            value: id.to_string(),
            label: clip_text(&redact_text(label), 240),
            description: None,
            preview: None,
        });
        option_labels.insert(id.to_string(), label.to_string());
    }

    let prompt = tool
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| {
            block
                .pointer("/content/text")
                .or_else(|| block.get("text"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if prompt.is_empty() {
        return None;
    }
    let prompt = clip_text(&redact_text(&prompt), MAX_KIMI_PLAN_REVIEW_CHARS);
    Some((
        HostQuestion {
            id: "kimi_plan_review".to_string(),
            header: Some("Kimi Code plan review".to_string()),
            prompt: prompt.clone(),
            kind: HostQuestionKind::SingleChoice,
            required: true,
            options,
        },
        PendingQuestion {
            id: "kimi_plan_review".to_string(),
            prompt,
            option_labels,
            option_previews: HashMap::new(),
            multi_select: false,
        },
    ))
}

async fn handle_kimi_permission_question(frame: &Value, params: &Value, context: &ReaderContext) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let Some((question, pending_question)) = kimi_permission_question(params) else {
        reply_rpc_error(
            &context.writer,
            raw_id,
            -32_602,
            "Kimi question payload was not understood",
        )
        .await;
        return;
    };
    let req_id = rpc_id_string(&raw_id);
    if !queue_host_request(
        &context.approvals,
        &context.interaction_sessions,
        req_id.clone(),
        params,
        PendingHostRequest::UserInput {
            raw_id: raw_id.clone(),
            questions: vec![pending_question],
            flavor: UserInputFlavor::KimiPermissionQuestion,
        },
    )
    .await
    {
        reply_rpc_error(
            &context.writer,
            raw_id,
            -32_000,
            "too many pending host requests",
        )
        .await;
        return;
    }
    emit_event(
        &context.event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::UserInput {
                questions: vec![question],
                metadata: json!({"transport":"kimi_permission_question_v1"}),
            },
        },
    )
    .await;
}

async fn handle_kimi_plan_review(
    frame: &Value,
    params: &Value,
    context: &ReaderContext,
    question: HostQuestion,
    pending_question: PendingQuestion,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    if context.permissions == BasePermissionProfile::Plan {
        let _ = write_permission_response(&context.writer, &raw_id, None, None).await;
        return;
    }
    let req_id = rpc_id_string(&raw_id);
    if !queue_host_request(
        &context.approvals,
        &context.interaction_sessions,
        req_id.clone(),
        params,
        PendingHostRequest::UserInput {
            raw_id: raw_id.clone(),
            questions: vec![pending_question],
            flavor: UserInputFlavor::KimiPlanReview,
        },
    )
    .await
    {
        let _ = write_permission_response(&context.writer, &raw_id, None, None).await;
        return;
    }
    emit_event(
        &context.event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::UserInput {
                questions: vec![question],
                metadata: json!({
                    "transport":"kimi_plan_review_permission_v1",
                    "responseContract":"kimi_plan_review_permission_v1"
                }),
            },
        },
    )
    .await;
}

async fn handle_kimi_plan_review_request(frame: &Value, params: &Value, context: &ReaderContext) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let Some((question, pending_question)) = kimi_plan_review_permission(params) else {
        reply_rpc_error(
            &context.writer,
            raw_id,
            -32_602,
            "Kimi plan-review payload was not understood",
        )
        .await;
        return;
    };
    handle_kimi_plan_review(frame, params, context, question, pending_question).await;
}

async fn handle_permission_request(
    frame: &Value,
    params: &Value,
    context: &ReaderContext,
    upstream_permission_boundary: bool,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let options = if context.vendor == AcpVendor::Kimi {
        let Some(options) = kimi_ordinary_permission_options(params) else {
            reply_rpc_error(
                &context.writer,
                raw_id,
                -32_602,
                "Kimi ordinary permission options did not match the audited contract",
            )
            .await;
            return;
        };
        options
    } else {
        parse_approval_options(params)
    };
    let record = ApprovalRecord {
        raw_id: raw_id.clone(),
        options: options.clone(),
    };
    match context.permissions {
        BasePermissionProfile::Plan => {
            let option = record.option_for_decision(ApprovalDecision::Deny);
            let _ =
                write_permission_response(&context.writer, &raw_id, option.as_deref(), None).await;
        }
        BasePermissionProfile::Guarded | BasePermissionProfile::Auto => {
            let req_id = rpc_id_string(&raw_id);
            let mut map = context.approvals.lock().await;
            if map.len() >= MAX_PENDING_APPROVALS || map.contains_key(&req_id) {
                drop(map);
                if context.permissions == BasePermissionProfile::Auto {
                    let _ = write_cancelled_host_response(
                        &context.writer,
                        PendingHostRequest::Approval(record),
                        None,
                    )
                    .await;
                } else {
                    let option = record.option_for_decision(ApprovalDecision::Deny);
                    let _ = write_permission_response(
                        &context.writer,
                        &raw_id,
                        option.as_deref(),
                        None,
                    )
                    .await;
                }
                return;
            }
            map.insert(req_id.clone(), PendingHostRequest::Approval(record));
            drop(map);
            remember_interaction_owner(&context.interaction_sessions, &req_id, params).await;
            let tool = params.get("toolCall").unwrap_or(&Value::Null);
            let input = sanitize_value(
                tool.get("rawInput")
                    .cloned()
                    .unwrap_or_else(|| tool.clone()),
            );
            emit_event(
                &context.event_tx,
                SessionEvent::HostRequest {
                    req_id,
                    request: HostRequest::Approval {
                        action: tool_name(tool),
                        target: tool_target(&input),
                        message: tool
                            .get("title")
                            .and_then(Value::as_str)
                            .map(|text| clip_text(text, 240)),
                        options,
                        metadata: if context.permissions == BasePermissionProfile::Auto
                            && upstream_permission_boundary
                        {
                            json!({
                                "toolCallId": tool.get("toolCallId").cloned().unwrap_or(Value::Null),
                                "requestedProfile":"auto",
                                "upstreamPermissionBoundary":true
                            })
                        } else if context.permissions == BasePermissionProfile::Auto {
                            // Kimi intentionally stays in its source-audited
                            // `default` mode so ACP keeps exposing every approval
                            // to UmaDev. The resident trust policy then auto-allows
                            // ordinary in-tree work and still escalates the
                            // irreversible floor. Marking this as an upstream
                            // boundary would paradoxically make Auto prompt more
                            // often than Guarded.
                            json!({
                                "toolCallId": tool.get("toolCallId").cloned().unwrap_or(Value::Null),
                                "requestedProfile":"auto",
                                "locallyMediated":true
                            })
                        } else {
                            json!({
                                "toolCallId": tool.get("toolCallId").cloned().unwrap_or(Value::Null),
                                "requestedProfile":"guarded"
                            })
                        },
                    },
                },
            )
            .await;
        }
    }
}

async fn handle_folder_trust_request(
    frame: &Value,
    params: &Value,
    context: &ReaderContext,
    replaying: bool,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let reject = || folder_trust_response_frame(&raw_id, FolderTrustUserDecision::KeepGated);

    // Folder Trust is a human-only vendor extension. A headless opener, replay
    // window, unknown peer, or unbound session must never manufacture trust.
    if replaying
        || !context.grok_source_contract.load(Ordering::Acquire)
        || !matches!(
            context.folder_trust_surface,
            FolderTrustClientSurface::Interactive
        )
    {
        let _ = write_json_line(&context.writer, &reject()).await;
        return;
    }

    let scope = context
        .folder_trust_scope
        .read()
        .ok()
        .and_then(|scope| scope.clone());
    let Some(scope) = scope else {
        // Grok spawns the interactive request from inside session/new and may
        // write it before the session/new response reaches this reader. The
        // returned session id is the only authority we can bind to, so retain a
        // tiny bounded copy during that handshake instead of either trusting
        // the request's self-asserted id or rejecting a legitimate prompt.
        if context
            .fresh_session_bind_in_progress
            .load(Ordering::Acquire)
        {
            let mut deferred = context.deferred_folder_trust_requests.lock().await;
            if deferred.len() < MAX_DEFERRED_FOLDER_TRUST_REQUESTS {
                deferred.push_back((frame.clone(), params.clone()));
                return;
            }
        }
        let _ = write_json_line(&context.writer, &reject()).await;
        return;
    };
    surface_bound_folder_trust_request(
        frame,
        params,
        &scope,
        &context.writer,
        &context.approvals,
        &context.interaction_sessions,
        &context.event_tx,
    )
    .await;
}

async fn surface_bound_folder_trust_request(
    frame: &Value,
    params: &Value,
    scope: &FolderTrustScope,
    writer: &SharedWriter,
    approvals: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    event_tx: &mpsc::Sender<SessionEvent>,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let reject = || folder_trust_response_frame(&raw_id, FolderTrustUserDecision::KeepGated);
    let Ok(request) = FolderTrustRequest::parse_for_scope(params, scope) else {
        let _ = write_json_line(writer, &reject()).await;
        return;
    };

    let req_id = rpc_id_string(&raw_id);
    let timeout_token = Arc::new(());
    if !queue_host_request(
        approvals,
        interaction_sessions,
        req_id.clone(),
        params,
        PendingHostRequest::FolderTrust {
            raw_id: raw_id.clone(),
            timeout_token: Arc::clone(&timeout_token),
        },
    )
    .await
    {
        let _ = write_json_line(writer, &reject()).await;
        return;
    }

    schedule_folder_trust_timeout(
        writer,
        approvals,
        interaction_sessions,
        req_id.clone(),
        timeout_token,
    );
    emit_event(
        event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::FolderTrust {
                cwd: request.cwd().to_path_buf(),
                workspace: request.workspace().to_path_buf(),
                config_kinds: request.config_kinds().to_vec(),
            },
        },
    )
    .await;
}

fn schedule_folder_trust_timeout(
    writer: &SharedWriter,
    approvals: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    req_id: String,
    timeout_token: Arc<()>,
) {
    let writer = Arc::downgrade(writer);
    let approvals = Arc::downgrade(approvals);
    let interaction_sessions = Arc::downgrade(interaction_sessions);
    tokio::spawn(async move {
        tokio::time::sleep(GROK_FOLDER_TRUST_TIMEOUT).await;
        let (Some(writer), Some(approvals), Some(interaction_sessions)) = (
            writer.upgrade(),
            approvals.upgrade(),
            interaction_sessions.upgrade(),
        ) else {
            return;
        };
        let pending = {
            let mut pending = approvals.lock().await;
            let is_same_request = matches!(
                pending.get(&req_id),
                Some(PendingHostRequest::FolderTrust {
                    timeout_token: current,
                    ..
                }) if Arc::ptr_eq(current, &timeout_token)
            );
            is_same_request.then(|| pending.remove(&req_id)).flatten()
        };
        if let Some(pending) = pending {
            interaction_sessions.lock().await.remove(&req_id);
            let _ = write_cancelled_host_response(&writer, pending, None).await;
        }
    });
}

async fn handle_user_input_request(
    frame: &Value,
    method: &str,
    normalized_params: &Value,
    writer: &SharedWriter,
    pending: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    event_tx: &mpsc::Sender<SessionEvent>,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let is_grok = is_grok_ask_user_question_method(method);
    if is_grok {
        if let Err(message) = validate_grok_ask_user_question(normalized_params) {
            reply_rpc_error(writer, raw_id, -32_602, message).await;
            return;
        }
    }
    let params = sanitize_value(normalized_params.clone());
    let questions = parse_host_questions(&params);
    if questions.is_empty() {
        reply_rpc_error(
            writer,
            raw_id,
            -32_602,
            "question payload was not understood",
        )
        .await;
        return;
    }
    let req_id = rpc_id_string(&raw_id);
    let pending_questions = questions
        .iter()
        .map(|question| PendingQuestion {
            id: question.id.clone(),
            prompt: question.prompt.clone(),
            option_labels: question
                .options
                .iter()
                .flat_map(|option| {
                    [
                        (option.value.clone(), option.label.clone()),
                        (option.label.clone(), option.label.clone()),
                    ]
                })
                .collect(),
            option_previews: question
                .options
                .iter()
                .filter_map(|option| {
                    option.preview.as_ref().map(|preview| {
                        [
                            (option.value.clone(), preview.clone()),
                            (option.label.clone(), preview.clone()),
                        ]
                    })
                })
                .flatten()
                .collect(),
            multi_select: matches!(question.kind, HostQuestionKind::MultiChoice),
        })
        .collect();
    let request = PendingHostRequest::UserInput {
        raw_id: raw_id.clone(),
        questions: pending_questions,
        flavor: if is_grok {
            UserInputFlavor::GrokAskUserQuestion
        } else {
            UserInputFlavor::Generic
        },
    };
    if !queue_host_request(
        pending,
        interaction_sessions,
        req_id.clone(),
        normalized_params,
        request,
    )
    .await
    {
        reply_rpc_error(writer, raw_id, -32_000, "too many pending host requests").await;
        return;
    }
    emit_event(
        event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::UserInput {
                questions,
                metadata: if is_grok {
                    json!({
                        "toolCallId": params.get("toolCallId").cloned().unwrap_or(Value::Null),
                        "mode": params.get("mode").cloned().unwrap_or(Value::Null),
                        "responseContract":"grok_ask_user_question_v1"
                    })
                } else {
                    Value::Null
                },
            },
        },
    )
    .await;
}

async fn handle_permission_expansion_request(
    frame: &Value,
    normalized_params: &Value,
    writer: &SharedWriter,
    pending: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    event_tx: &mpsc::Sender<SessionEvent>,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let params = sanitize_value(normalized_params.clone());
    let permissions = parse_host_permissions(&params);
    let req_id = rpc_id_string(&raw_id);
    if !queue_host_request(
        pending,
        interaction_sessions,
        req_id.clone(),
        normalized_params,
        PendingHostRequest::PermissionExpansion {
            raw_id: raw_id.clone(),
        },
    )
    .await
    {
        reply_rpc_error(writer, raw_id, -32_000, "too many pending host requests").await;
        return;
    }
    emit_event(
        event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::PermissionExpansion {
                permissions,
                reason: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, 512)),
                metadata: Value::Null,
            },
        },
    )
    .await;
}

async fn handle_elicitation_request(
    frame: &Value,
    normalized_params: &Value,
    writer: &SharedWriter,
    pending: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    event_tx: &mpsc::Sender<SessionEvent>,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let params = sanitize_value(normalized_params.clone());
    let req_id = rpc_id_string(&raw_id);
    if !queue_host_request(
        pending,
        interaction_sessions,
        req_id.clone(),
        normalized_params,
        PendingHostRequest::McpElicitation {
            raw_id: raw_id.clone(),
        },
    )
    .await
    {
        reply_rpc_error(writer, raw_id, -32_000, "too many pending host requests").await;
        return;
    }
    emit_event(
        event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::McpElicitation {
                server_name: params
                    .get("serverName")
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, 160)),
                message: params
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, 4096))
                    .unwrap_or_default(),
                requested_schema: params
                    .get("requestedSchema")
                    .or_else(|| params.get("schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"})),
                metadata: Value::Null,
            },
        },
    )
    .await;
}

async fn handle_plan_confirmation_request(
    frame: &Value,
    method: &str,
    normalized_params: &Value,
    writer: &SharedWriter,
    pending: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    event_tx: &mpsc::Sender<SessionEvent>,
) {
    let raw_id = frame.get("id").cloned().unwrap_or(Value::Null);
    let is_grok = is_grok_exit_plan_mode_method(method);
    if is_grok {
        if let Err(message) = validate_grok_exit_plan_mode(normalized_params) {
            reply_rpc_error(writer, raw_id, -32_602, message).await;
            return;
        }
    }
    let params = sanitize_value(normalized_params.clone());
    let req_id = rpc_id_string(&raw_id);
    if !queue_host_request(
        pending,
        interaction_sessions,
        req_id.clone(),
        normalized_params,
        PendingHostRequest::PlanConfirmation {
            raw_id: raw_id.clone(),
            flavor: if is_grok {
                PlanConfirmationFlavor::GrokExitPlanMode
            } else {
                PlanConfirmationFlavor::Generic
            },
        },
    )
    .await
    {
        reply_rpc_error(writer, raw_id, -32_000, "too many pending host requests").await;
        return;
    }
    emit_event(
        event_tx,
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::PlanConfirmation {
                plan: params
                    .get("planContent")
                    .or_else(|| params.get("plan"))
                    .or_else(|| params.get("content"))
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, 32 * 1024))
                    .unwrap_or_default(),
                message: params
                    .get("message")
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, 512)),
                metadata: if is_grok {
                    json!({
                        "toolCallId": params.get("toolCallId").cloned().unwrap_or(Value::Null),
                        "responseContract":"grok_exit_plan_mode_v1"
                    })
                } else {
                    Value::Null
                },
            },
        },
    )
    .await;
}

async fn queue_host_request(
    pending: &ApprovalMap,
    interaction_sessions: &InteractionSessions,
    req_id: String,
    params: &Value,
    request: PendingHostRequest,
) -> bool {
    let mut map = pending.lock().await;
    if map.len() >= MAX_PENDING_APPROVALS || map.contains_key(&req_id) {
        false
    } else {
        map.insert(req_id.clone(), request);
        drop(map);
        remember_interaction_owner(interaction_sessions, &req_id, params).await;
        true
    }
}

async fn remember_interaction_owner(
    interaction_sessions: &InteractionSessions,
    req_id: &str,
    params: &Value,
) {
    let Some(session_id) = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(Value::as_str)
    else {
        return;
    };
    interaction_sessions
        .lock()
        .await
        .insert(req_id.to_string(), session_id.to_string());
}

async fn emit_event(event_tx: &mpsc::Sender<SessionEvent>, event: SessionEvent) {
    match event {
        // High-volume presentation deltas are intentionally lossy. Every other
        // event carries session, tool, approval, lifecycle, or terminal state and
        // therefore must apply bounded backpressure instead of disappearing when
        // the 256-slot queue is full.
        SessionEvent::TextDelta(_)
        | SessionEvent::ThinkingDelta(_)
        | SessionEvent::ToolOutputDelta(_)
        | SessionEvent::ToolOutputDeltaCorrelated { .. } => {
            let _ = event_tx.try_send(event);
        }
        _ => {
            let _ = event_tx.send(event).await;
        }
    }
}

async fn emit_converged_terminal(
    event_tx: &mpsc::Sender<SessionEvent>,
    turn_active: &AtomicBool,
    terminal: Option<ConvergedTerminal>,
) {
    let Some(terminal) = terminal else {
        return;
    };
    if turn_active.swap(false, Ordering::AcqRel) {
        emit_event(
            event_tx,
            SessionEvent::TurnDone {
                status: terminal.status,
                usage: terminal.usage,
            },
        )
        .await;
    }
}

#[derive(Default)]
struct ToolState {
    known: HashSet<String>,
    /// Kimi and other ACP agents may create a pending tool from streamed JSON
    /// before parsed `rawInput` exists. Hold those ids until the authoritative
    /// started-upgrade arrives so consumers see one factual tool call, not a
    /// placeholder followed by arguments mislabelled as command output.
    provisional: HashSet<String>,
    settled: HashSet<String>,
    seen_grok_event_ids: HashSet<String>,
    grok_event_order: VecDeque<String>,
    bash_streams: HashMap<String, GrokBashStream>,
}

#[derive(Debug, Default)]
struct GrokBashStream {
    sanitizer: TerminalTextSanitizer,
}

impl GrokBashStream {
    fn reset(&mut self) {
        self.sanitizer.reset();
    }

    fn append(&mut self, bytes: &[u8]) -> String {
        if bytes.len() > MAX_BASH_RENDER_BYTES {
            self.sanitizer.reset();
            let tail = &bytes[bytes.len() - MAX_BASH_RENDER_BYTES..];
            let mut text = "[terminal output clipped]\n".to_string();
            text.push_str(&self.sanitizer.push(tail));
            return text;
        }
        self.sanitizer.push(bytes)
    }

    fn replace(&mut self, bytes: &[u8]) -> String {
        self.reset();
        self.append(bytes)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum GrokBashUpdate {
    Append(String),
    Snapshot(String),
}

impl ToolState {
    fn remember_grok_event(&mut self, event_id: &str) -> bool {
        if event_id.is_empty() || self.seen_grok_event_ids.contains(event_id) {
            return false;
        }
        self.seen_grok_event_ids.insert(event_id.to_string());
        self.grok_event_order.push_back(event_id.to_string());
        while self.grok_event_order.len() > MAX_SEEN_GROK_EVENT_IDS {
            if let Some(expired) = self.grok_event_order.pop_front() {
                self.seen_grok_event_ids.remove(&expired);
            }
        }
        true
    }
}

#[cfg(test)]
fn grok_subagent_lifecycle_event(
    params: &Value,
    state: &mut ToolState,
    grok_source_contract: bool,
) -> Option<SessionEvent> {
    if !grok_source_contract {
        return None;
    }
    let update = params.get("update")?;
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))?
        .as_str()?;
    // Progress is useful presentation data but not a lifecycle edge. UmaDev
    // intentionally promises Lifecycle, never an authoritative live set.
    if kind == "subagent_progress" {
        return None;
    }
    let subagent_id = update
        .get("subagentId")
        .or_else(|| update.get("subagent_id"))?
        .as_str()?
        .trim();
    let event_id = params
        .pointer("/_meta/eventId")
        .or_else(|| params.pointer("/_meta/event_id"))?
        .as_str()?
        .trim();
    if subagent_id.is_empty()
        || subagent_id.len() > MAX_DIAGNOSTIC_CHARS
        || event_id.is_empty()
        || event_id.len() > MAX_DIAGNOSTIC_CHARS
    {
        return None;
    }
    let signal = match kind {
        "subagent_spawned" => BackgroundTaskSignal::Started {
            id: subagent_id.to_string(),
        },
        "subagent_finished"
            if update
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| matches!(status, "completed" | "failed" | "cancelled")) =>
        {
            BackgroundTaskSignal::Finished {
                id: subagent_id.to_string(),
            }
        }
        _ => return None,
    };
    state
        .remember_grok_event(event_id)
        .then_some(SessionEvent::BackgroundTask(signal))
}

fn grok_background_process_lifecycle_event(
    method: &str,
    params: &Value,
    state: &mut ToolState,
    grok_source_contract: bool,
) -> Option<ParsedBackgroundProcess> {
    if !grok_source_contract {
        return None;
    }
    let update = params.get("update")?;
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))?
        .as_str()?;
    if !matches!(
        (method, kind),
        ("x.ai/task_backgrounded", "task_backgrounded")
            | ("x.ai/task_completed", "task_completed")
            | (
                "x.ai/session_notification" | "x.ai/session/update",
                "task_backgrounded" | "task_completed"
            )
    ) {
        return None;
    }

    let event_id = params
        .pointer("/_meta/eventId")
        .or_else(|| params.pointer("/_meta/event_id"))?
        .as_str()?;
    if !valid_background_id(event_id) {
        return None;
    }

    let parsed = match kind {
        "task_backgrounded" => parse_background_process_started(update),
        "task_completed" => parse_background_process_finished(update),
        _ => None,
    }?;
    state.remember_grok_event(event_id).then_some(parsed)
}

fn parse_background_process_started(update: &Value) -> Option<ParsedBackgroundProcess> {
    let raw_task_id =
        bounded_required_string(update, "task_id", "taskId", MAX_BACKGROUND_ID_CHARS)?;
    let tool_call_id = bounded_required_string(
        update,
        "tool_call_id",
        "toolCallId",
        MAX_BACKGROUND_ID_CHARS,
    )?;
    if !valid_background_id(raw_task_id) || !valid_background_id(tool_call_id) {
        return None;
    }
    let command =
        bounded_required_string(update, "command", "command", MAX_BACKGROUND_COMMAND_CHARS)?;
    let cwd = bounded_required_string(update, "cwd", "cwd", MAX_BACKGROUND_PATH_CHARS)?;
    let output_file = bounded_required_string(
        update,
        "output_file",
        "outputFile",
        MAX_BACKGROUND_PATH_CHARS,
    )?;
    if command.is_empty() || cwd.is_empty() || output_file.is_empty() {
        return None;
    }
    let monitor_description = bounded_optional_string(
        update,
        "monitor_description",
        "monitorDescription",
        MAX_BACKGROUND_DESCRIPTION_CHARS,
    )?
    .into_option();
    let description = bounded_optional_string(
        update,
        "description",
        "description",
        MAX_BACKGROUND_DESCRIPTION_CHARS,
    )?
    .into_option();
    let process_kind = if monitor_description.is_some() {
        BackgroundProcessKind::Monitor
    } else {
        BackgroundProcessKind::Bash
    };
    let description = monitor_description
        .or(description)
        .filter(|text| !text.trim().is_empty())
        .map(redact_text);
    Some(ParsedBackgroundProcess::Started {
        raw_task_id: raw_task_id.to_string(),
        process: BackgroundProcessInfo {
            task_id: redact_text(raw_task_id),
            tool_call_id: redact_text(tool_call_id),
            kind: process_kind,
            description,
        },
    })
}

fn parse_background_process_finished(update: &Value) -> Option<ParsedBackgroundProcess> {
    let snapshot = update
        .get("task_snapshot")
        .or_else(|| update.get("taskSnapshot"))?
        .as_object()?;
    let snapshot = Value::Object(snapshot.clone());
    let raw_task_id =
        bounded_required_string(&snapshot, "task_id", "taskId", MAX_BACKGROUND_ID_CHARS)?;
    if !valid_background_id(raw_task_id) {
        return None;
    }
    let command = bounded_required_string(
        &snapshot,
        "command",
        "command",
        MAX_BACKGROUND_COMMAND_CHARS,
    )?;
    let cwd = bounded_required_string(&snapshot, "cwd", "cwd", MAX_BACKGROUND_PATH_CHARS)?;
    let output_file = bounded_required_string(
        &snapshot,
        "output_file",
        "outputFile",
        MAX_BACKGROUND_PATH_CHARS,
    )?;
    snapshot.get("output")?.as_str()?;
    if command.is_empty()
        || cwd.is_empty()
        || output_file.is_empty()
        || !valid_wire_system_time(
            snapshot
                .get("start_time")
                .or_else(|| snapshot.get("startTime"))?,
        )
        || !valid_optional_wire_system_time(
            snapshot.get("end_time").or_else(|| snapshot.get("endTime")),
        )
    {
        return None;
    }
    bounded_optional_string(
        &snapshot,
        "display_command",
        "displayCommand",
        MAX_BACKGROUND_COMMAND_CHARS,
    )?;
    bounded_optional_string(
        &snapshot,
        "owner_session_id",
        "ownerSessionId",
        MAX_BACKGROUND_ID_CHARS,
    )?;
    for (snake, camel) in [
        ("block_waited", "blockWaited"),
        ("explicitly_killed", "explicitlyKilled"),
    ] {
        if field(&snapshot, snake, camel).is_some_and(|value| !value.is_boolean()) {
            return None;
        }
    }
    if snapshot.get("completed").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let truncated = snapshot.get("truncated")?.as_bool()?;
    let process_kind = match snapshot
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("bash")
    {
        "bash" => BackgroundProcessKind::Bash,
        "monitor" => BackgroundProcessKind::Monitor,
        _ => return None,
    };
    let exit_code = match field(&snapshot, "exit_code", "exitCode") {
        None | Some(Value::Null) => None,
        Some(value) => Some(i32::try_from(value.as_i64()?).ok()?),
    };
    let signal =
        bounded_optional_string(&snapshot, "signal", "signal", MAX_BACKGROUND_SIGNAL_CHARS)?
            .into_option()
            .map(redact_text);
    let will_wake = match field(update, "will_wake", "willWake") {
        None => false,
        Some(value) => value.as_bool()?,
    };
    Some(ParsedBackgroundProcess::Finished {
        raw_task_id: raw_task_id.to_string(),
        task_id: redact_text(raw_task_id),
        kind: process_kind,
        exit_code,
        signal,
        truncated,
        will_wake,
    })
}

fn field<'a>(value: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    value.get(snake).or_else(|| value.get(camel))
}

fn bounded_required_string<'a>(
    value: &'a Value,
    snake: &str,
    camel: &str,
    max_chars: usize,
) -> Option<&'a str> {
    let text = field(value, snake, camel)?.as_str()?;
    (text.chars().count() <= max_chars).then_some(text)
}

enum BoundedOptionalString<'a> {
    Absent,
    Present(&'a str),
}

impl<'a> BoundedOptionalString<'a> {
    fn into_option(self) -> Option<&'a str> {
        match self {
            Self::Absent => None,
            Self::Present(value) => Some(value),
        }
    }
}

fn bounded_optional_string<'a>(
    value: &'a Value,
    snake: &str,
    camel: &str,
    max_chars: usize,
) -> Option<BoundedOptionalString<'a>> {
    match field(value, snake, camel) {
        None | Some(Value::Null) => Some(BoundedOptionalString::Absent),
        Some(value) => {
            let text = value.as_str()?;
            (text.chars().count() <= max_chars).then_some(BoundedOptionalString::Present(text))
        }
    }
}

fn valid_background_id(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && value.chars().count() <= MAX_BACKGROUND_ID_CHARS
}

fn valid_wire_system_time(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object
        .get("secs_since_epoch")
        .and_then(Value::as_u64)
        .is_some()
        && object
            .get("nanos_since_epoch")
            .and_then(Value::as_u64)
            .is_some_and(|nanos| nanos < 1_000_000_000)
}

fn valid_optional_wire_system_time(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(value) => valid_wire_system_time(value),
    }
}

fn valid_session_state_id(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.chars().count() <= MAX_SESSION_STATE_ID_CHARS
}

fn valid_optional_session_model_id(value: &str) -> bool {
    value.is_empty() || valid_session_state_id(value)
}

fn bounded_state_text(value: &Value) -> Option<String> {
    let value = value.as_str()?;
    (value.chars().count() <= MAX_SESSION_STATE_TEXT_CHARS).then(|| redact_text(value))
}

fn optional_bounded_state_text(value: Option<&Value>) -> Result<Option<String>, &'static str> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => bounded_state_text(value)
            .map(Some)
            .ok_or("session state text is not a bounded string"),
    }
}

fn parse_initialize_model_catalog(initialize: &Value) -> Option<SessionStateUpdate> {
    parse_model_catalog(initialize.pointer("/_meta/modelState")?)
}

fn parse_setup_model_catalog(setup: &Value) -> Option<SessionStateUpdate> {
    setup
        .get("models")
        .and_then(parse_model_catalog)
        .or_else(|| parse_config_option_model_catalog(setup))
}

fn config_option<'a>(value: &'a Value, wanted: &str) -> Option<&'a Value> {
    value
        .get("configOptions")
        .or_else(|| value.get("config_options"))
        .and_then(Value::as_array)?
        .iter()
        .find(|option| {
            option
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == wanted)
        })
}

fn extract_config_option_current<'a>(value: &'a Value, id: &str) -> Option<&'a str> {
    config_option(value, id)?
        .get("currentValue")
        .or_else(|| config_option(value, id)?.get("current_value"))
        .and_then(Value::as_str)
        .filter(|current| valid_session_state_id(current))
}

fn parse_config_option_model_catalog(setup: &Value) -> Option<SessionStateUpdate> {
    let model = config_option(setup, "model")?;
    if model.get("type").and_then(Value::as_str) != Some("select") {
        return None;
    }
    let current_model_id = extract_config_option_current(setup, "model")?;
    let options = model.get("options")?.as_array()?;
    if options.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }
    let available_models = options
        .iter()
        .map(|option| {
            let model_id = option.get("value")?.as_str()?;
            let name = option.get("name")?.as_str()?;
            if !valid_session_state_id(model_id)
                || name.chars().count() > MAX_SESSION_STATE_TEXT_CHARS
            {
                return None;
            }
            Some(SessionModelInfo {
                model_id: model_id.to_string(),
                name: redact_text(name),
                description: optional_bounded_state_text(option.get("description")).ok()?,
                total_context_tokens: None,
                agent_type: None,
                // Kimi's separate `thinking` select is boolean, not the
                // runtime's graded reasoning-effort axis; do not mislabel it.
                supports_reasoning_effort: false,
                reasoning_effort: None,
                reasoning_efforts: Vec::new(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(SessionStateUpdate::ModelCatalogReplaced {
        current_model_id: current_model_id.to_string(),
        available_models,
    })
}

fn parse_model_catalog(value: &Value) -> Option<SessionStateUpdate> {
    let current_model_id = value.get("currentModelId")?.as_str()?;
    if !valid_session_state_id(current_model_id) {
        return None;
    }
    let available = value.get("availableModels")?.as_array()?;
    if available.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }
    let available_models = available
        .iter()
        .map(parse_session_model_info)
        .collect::<Option<Vec<_>>>()?;
    Some(SessionStateUpdate::ModelCatalogReplaced {
        current_model_id: current_model_id.to_string(),
        available_models,
    })
}

fn parse_session_model_info(value: &Value) -> Option<SessionModelInfo> {
    let model_id = value.get("modelId")?.as_str()?;
    let name = value.get("name")?.as_str()?;
    if !valid_session_state_id(model_id) || name.chars().count() > MAX_SESSION_STATE_TEXT_CHARS {
        return None;
    }
    let description = optional_bounded_state_text(value.get("description")).ok()?;
    let meta = match value.get("_meta") {
        None | Some(Value::Null) => None,
        Some(Value::Object(meta)) => Some(meta),
        Some(_) => return None,
    };
    let total_context_tokens = meta
        .and_then(|meta| meta.get("totalContextTokens"))
        .and_then(Value::as_u64);
    let agent_type = meta
        .and_then(|meta| optional_bounded_state_text(meta.get("agentType")).ok())
        .flatten();
    let supports_reasoning_effort = meta
        .and_then(|meta| meta.get("supportsReasoningEffort"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reasoning_effort = meta
        .and_then(|meta| meta.get("reasoningEffort"))
        .and_then(Value::as_str)
        .and_then(|value| SessionReasoningEffort::try_from(value).ok());
    let reasoning_efforts = meta
        .and_then(|meta| meta.get("reasoningEfforts"))
        .and_then(parse_reasoning_effort_options)
        .unwrap_or_default();
    Some(SessionModelInfo {
        model_id: model_id.to_string(),
        name: redact_text(name),
        description,
        total_context_tokens,
        agent_type,
        supports_reasoning_effort,
        reasoning_effort,
        reasoning_efforts,
    })
}

fn parse_reasoning_effort_options(value: &Value) -> Option<Vec<SessionReasoningEffortOption>> {
    let values = value.as_array()?;
    if values.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }
    Some(
        values
            .iter()
            .filter_map(parse_reasoning_effort_option)
            .collect(),
    )
}

fn parse_reasoning_effort_option(value: &Value) -> Option<SessionReasoningEffortOption> {
    if let Some(value) = value.as_str() {
        let effort = SessionReasoningEffort::try_from(value).ok()?;
        return Some(SessionReasoningEffortOption {
            id: effort.as_str().to_string(),
            value: effort,
            label: humanize_session_state_id(effort.as_str()),
            description: None,
            default: false,
        });
    }
    let object = value.as_object()?;
    let effort = object
        .get("value")
        .and_then(Value::as_str)
        .and_then(|value| SessionReasoningEffort::try_from(value).ok())?;
    let id = match object.get("id") {
        None | Some(Value::Null) => effort.as_str().to_string(),
        Some(value) => value.as_str()?.to_string(),
    };
    if !valid_session_state_id(&id) {
        return None;
    }
    let label = match object.get("label") {
        None | Some(Value::Null) => humanize_session_state_id(&id),
        Some(value) => bounded_state_text(value)?,
    };
    let description = optional_bounded_state_text(object.get("description")).ok()?;
    let default = match object.get("default") {
        None | Some(Value::Null) => false,
        Some(value) => value.as_bool()?,
    };
    Some(SessionReasoningEffortOption {
        id,
        value: effort,
        label,
        description,
        default,
    })
}

fn humanize_session_state_id(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn parse_initialize_command_catalog(initialize: &Value) -> Option<SessionStateUpdate> {
    parse_command_catalog(initialize.pointer("/_meta/availableCommands")?, None)
}

fn parse_standard_command_catalog(update: &Value) -> Option<SessionStateUpdate> {
    parse_command_catalog(
        update
            .get("availableCommands")
            .or_else(|| update.get("available_commands"))?,
        update.pointer("/_meta/tools"),
    )
}

fn parse_standard_plan(update: &Value) -> Option<SessionStateUpdate> {
    let entries = update.get("entries")?.as_array()?;
    if entries.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }
    let entries = entries
        .iter()
        .map(|entry| {
            let object = entry.as_object()?;
            let content = bounded_state_text(object.get("content")?)?;
            if content.trim().is_empty() {
                return None;
            }
            let priority = match object.get("priority")?.as_str()? {
                "high" => SessionPlanEntryPriority::High,
                "medium" => SessionPlanEntryPriority::Medium,
                "low" => SessionPlanEntryPriority::Low,
                _ => return None,
            };
            let status = match object.get("status")?.as_str()? {
                "pending" => SessionPlanEntryStatus::Pending,
                "in_progress" => SessionPlanEntryStatus::InProgress,
                "completed" => SessionPlanEntryStatus::Completed,
                _ => return None,
            };
            Some(SessionPlanEntry {
                content,
                priority,
                status,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(SessionStateUpdate::PlanReplaced { entries })
}

fn parse_command_catalog(commands: &Value, tools: Option<&Value>) -> Option<SessionStateUpdate> {
    let commands = commands.as_array()?;
    if commands.len() > MAX_SESSION_STATE_ITEMS {
        return None;
    }
    let commands = commands
        .iter()
        .map(parse_session_command_info)
        .collect::<Option<Vec<_>>>()?;
    let tools = match tools {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let values = value.as_array()?;
            if values.len() > MAX_SESSION_STATE_ITEMS {
                return None;
            }
            values
                .iter()
                .map(|value| {
                    let value = value.as_str()?;
                    valid_session_state_id(value).then(|| value.to_string())
                })
                .collect::<Option<Vec<_>>>()?
        }
    };
    Some(SessionStateUpdate::CommandCatalogReplaced { commands, tools })
}

fn parse_session_command_info(value: &Value) -> Option<SessionCommandInfo> {
    let object = value.as_object()?;
    let name = object.get("name")?.as_str()?;
    let description = object.get("description")?.as_str()?;
    if !valid_session_state_id(name) || description.chars().count() > MAX_SESSION_STATE_TEXT_CHARS {
        return None;
    }
    let input_hint = match object.get("input") {
        None | Some(Value::Null) => None,
        Some(Value::Object(input)) => Some(bounded_state_text(input.get("hint")?)?),
        Some(_) => return None,
    };
    let meta = match object.get("_meta") {
        None | Some(Value::Null) => None,
        Some(Value::Object(meta)) => Some(meta),
        Some(_) => return None,
    };
    let scope = meta
        .and_then(|meta| optional_bounded_state_text(meta.get("scope")).ok())
        .flatten();
    let source_path = meta
        .and_then(|meta| optional_bounded_state_text(meta.get("path")).ok())
        .flatten();
    Some(SessionCommandInfo {
        name: name.to_string(),
        description: redact_text(description),
        input_hint,
        scope,
        source_path,
    })
}

fn parse_current_mode_update(update: &Value) -> Option<SessionStateUpdate> {
    let mode = update
        .get("currentModeId")
        .or_else(|| update.get("current_mode_id"))?
        .as_str()
        .and_then(|mode| SessionMode::try_from(mode).ok())?;
    Some(SessionStateUpdate::ModeChanged { mode })
}

fn parse_grok_model_state_events(params: &Value) -> Vec<SessionEvent> {
    let Some(update) = params.get("update") else {
        return Vec::new();
    };
    let kind = update.get("sessionUpdate").and_then(Value::as_str);
    match kind {
        Some("model_changed") => {
            let Some(model_id) = update.get("model_id").and_then(Value::as_str) else {
                return Vec::new();
            };
            if !valid_session_state_id(model_id) {
                return Vec::new();
            }
            let reasoning_effort = match update.get("reasoning_effort") {
                None | Some(Value::Null) => None,
                Some(value) => {
                    let Some(effort) = value
                        .as_str()
                        .and_then(|value| SessionReasoningEffort::try_from(value).ok())
                    else {
                        return Vec::new();
                    };
                    Some(effort)
                }
            };
            vec![
                SessionEvent::StateUpdate(SessionStateUpdate::ModelChanged {
                    model_id: model_id.to_string(),
                    reasoning_effort,
                }),
                SessionEvent::SessionModel(model_id.to_string()),
            ]
        }
        Some("model_auto_switched") => {
            let Some(previous_model_id) = update.get("previous_model_id").and_then(Value::as_str)
            else {
                return Vec::new();
            };
            let Some(new_model_id) = update.get("new_model_id").and_then(Value::as_str) else {
                return Vec::new();
            };
            let Some(reason) = update.get("reason").and_then(bounded_state_text) else {
                return Vec::new();
            };
            if !valid_session_state_id(previous_model_id)
                || !valid_optional_session_model_id(new_model_id)
            {
                return Vec::new();
            }
            let mut events = vec![SessionEvent::StateUpdate(
                SessionStateUpdate::ModelAutoSwitched {
                    previous_model_id: previous_model_id.to_string(),
                    new_model_id: new_model_id.to_string(),
                    reason,
                },
            )];
            if !new_model_id.is_empty() {
                events.push(SessionEvent::SessionModel(new_model_id.to_string()));
            }
            events
        }
        _ => Vec::new(),
    }
}

fn parse_session_update(params: &Value, tools: &mut ToolState) -> Vec<SessionEvent> {
    let update = params.get("update").unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => text_from_content(update)
            .filter(|text| !text.is_empty())
            .map(SessionEvent::TextDelta)
            .into_iter()
            .collect(),
        "agent_thought_chunk" => text_from_content(update)
            .filter(|text| !text.is_empty())
            .map(SessionEvent::ThinkingDelta)
            .into_iter()
            .collect(),
        "tool_call" => parse_tool_call(update, tools),
        "tool_call_update" => parse_tool_update(update, tools),
        "config_option_update" => parse_config_option_state_events(update),
        "current_model_update" => extract_session_model(update)
            .map(SessionEvent::SessionModel)
            .into_iter()
            .collect(),
        "current_mode_update" => parse_current_mode_update(update)
            .map(SessionEvent::StateUpdate)
            .into_iter()
            .collect(),
        "available_commands_update" => parse_standard_command_catalog(update)
            .map(SessionEvent::StateUpdate)
            .into_iter()
            .collect(),
        "plan" => parse_standard_plan(update)
            .map(SessionEvent::StateUpdate)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_replay_session_state(params: &Value) -> Vec<SessionEvent> {
    let update = params.get("update").unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "config_option_update" => parse_config_option_state_events(update),
        "current_model_update" => extract_session_model(update)
            .map(SessionEvent::SessionModel)
            .into_iter()
            .collect(),
        "current_mode_update" => parse_current_mode_update(update)
            .map(SessionEvent::StateUpdate)
            .into_iter()
            .collect(),
        "available_commands_update" => parse_standard_command_catalog(update)
            .map(SessionEvent::StateUpdate)
            .into_iter()
            .collect(),
        "plan" => parse_standard_plan(update)
            .map(SessionEvent::StateUpdate)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

fn parse_config_option_state_events(update: &Value) -> Vec<SessionEvent> {
    let mut events = Vec::with_capacity(5);
    if let Some(catalog) = parse_config_option_model_catalog(update) {
        events.push(SessionEvent::StateUpdate(catalog));
    }
    if let Some(model) = extract_session_model(update) {
        events.push(SessionEvent::SessionModel(model));
    }
    events.push(SessionEvent::StateUpdate(parse_thinking_state(update)));
    if let Some(mode) = extract_config_option_current(update, "mode")
        .and_then(|mode| SessionMode::try_from(mode).ok())
    {
        events.push(SessionEvent::StateUpdate(SessionStateUpdate::ModeChanged {
            mode,
        }));
    }
    events
}

fn parse_thinking_state(update: &Value) -> SessionStateUpdate {
    let thinking = config_option(update, "thinking");
    let enabled = thinking
        .and_then(|option| {
            option
                .get("currentValue")
                .or_else(|| option.get("current_value"))
        })
        .and_then(Value::as_str)
        .and_then(|value| match value {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        });
    let selectable = |wanted: &str| {
        thinking
            .and_then(|option| option.get("options"))
            .and_then(Value::as_array)
            .is_some_and(|options| {
                options
                    .iter()
                    .any(|option| option.get("value").and_then(Value::as_str) == Some(wanted))
            })
    };
    SessionStateUpdate::ThinkingChanged {
        enabled,
        can_enable: selectable("on"),
        can_disable: selectable("off"),
    }
}

fn parse_tool_call(update: &Value, tools: &mut ToolState) -> Vec<SessionEvent> {
    let id = bounded_tool_call_id(update);
    let provisional = !id.is_empty()
        && update.get("status").and_then(Value::as_str) == Some("pending")
        && update
            .get("rawInput")
            .or_else(|| update.get("raw_input"))
            .is_none();
    if provisional {
        bounded_insert(&mut tools.provisional, id);
        return Vec::new();
    }
    if !id.is_empty() {
        bounded_insert(&mut tools.known, id.clone());
    }
    let input = normalized_tool_input(update);
    vec![if id.is_empty() {
        SessionEvent::ToolCall {
            name: tool_name(update),
            input,
        }
    } else {
        SessionEvent::ToolCallCorrelated {
            call_id: id,
            name: tool_name(update),
            input,
        }
    }]
}

fn parse_tool_update(update: &Value, tools: &mut ToolState) -> Vec<SessionEvent> {
    let id = bounded_tool_call_id(update);
    let mut events = Vec::new();
    let status = update.get("status").and_then(Value::as_str).unwrap_or("");
    let has_raw_input = update
        .get("rawInput")
        .or_else(|| update.get("raw_input"))
        .is_some();
    let was_provisional = !id.is_empty() && tools.provisional.contains(&id);
    let was_known = !id.is_empty() && tools.known.contains(&id);
    if was_provisional && !has_raw_input && !matches!(status, "completed" | "failed") {
        // Cumulative streamed arguments replace the provisional card's content.
        // They are neither process output nor an authoritative executable input.
        return events;
    }
    if !id.is_empty() && (was_provisional || !tools.known.contains(&id)) {
        tools.provisional.remove(&id);
        bounded_insert(&mut tools.known, id.clone());
        let input = normalized_tool_input(update);
        events.push(SessionEvent::ToolCallCorrelated {
            call_id: id.clone(),
            name: tool_name(update),
            input,
        });
    }
    if has_raw_input && !matches!(status, "completed" | "failed") {
        // A started-upgrade's content is the canonical argument/diff snapshot.
        // The ToolCall above carries it; emitting ToolOutput here would render
        // the JSON arguments as if the tool had already produced stdout.
        return events;
    }
    // Kimi Code maps SDK `tool.progress` status updates to a title-only ACP
    // `tool_call_update`. The title is a complete tool-card status replacement,
    // not stdout and not a terminal result. Preserve that distinction and the
    // stable call id so an interleaved update cannot land on a neighbouring
    // tool. A missing start is already recovered above as a synthetic ToolCall;
    // avoid emitting a duplicate title update for that same recovery frame.
    let progress_title = update
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty());
    if was_known
        && !was_provisional
        && !tools.settled.contains(&id)
        && !has_raw_input
        && !matches!(status, "completed" | "failed")
    {
        if let Some(title) = progress_title {
            events.push(SessionEvent::ToolProgressCorrelated {
                call_id: id.clone(),
                title: clip_text(&redact_text(title), MAX_DIAGNOSTIC_CHARS),
            });
        }
    }
    let bash_output = grok_bash_raw_output(update);
    let mut bash_update = bash_output.and_then(|raw| grok_bash_stream_delta(&id, raw, tools));
    let summary =
        bash_output.map_or_else(|| tool_output_summary(update), grok_bash_terminal_summary);
    if matches!(status, "completed" | "failed") && !id.is_empty() && tools.settled.contains(&id) {
        return events;
    }
    match status {
        "completed" | "failed" if id.is_empty() || !tools.settled.contains(&id) => {
            if !id.is_empty() {
                bounded_insert(&mut tools.settled, id.clone());
                if let Some(mut stream) = tools.bash_streams.remove(&id) {
                    append_grok_bash_tail(
                        &mut bash_update,
                        redact_text(&stream.sanitizer.finish()),
                    );
                }
            }
            if let Some(update) = bash_update {
                push_grok_bash_event(&mut events, &id, update);
            }
            if id.is_empty() {
                events.push(SessionEvent::ToolResult {
                    ok: status == "completed",
                    summary,
                });
            } else {
                events.push(SessionEvent::ToolResultCorrelated {
                    call_id: id,
                    ok: status == "completed",
                    summary,
                });
            }
        }
        _ if bash_output.is_some() => {
            if let Some(update) = bash_update {
                push_grok_bash_event(&mut events, &id, update);
            }
        }
        _ if bash_output.is_none() && !summary.is_empty() => {
            if id.is_empty() {
                events.push(SessionEvent::ToolOutputDelta(summary));
            } else {
                events.push(SessionEvent::ToolOutputDeltaCorrelated {
                    call_id: id,
                    delta: summary,
                });
            }
        }
        _ => {}
    }
    events
}

/// Normalize one ACP tool payload without discarding the protocol's structured
/// diff card. Kimi's official adapter carries before/after in `content[type=diff]`
/// even when the tool's own `rawInput` uses a different schema. Projecting the
/// missing fields into the existing Write/Edit vocabulary lets the TUI render a
/// real diff and lets governance inspect the proposed content.
fn normalized_tool_input(update: &Value) -> Value {
    let mut input = update
        .get("rawInput")
        .or_else(|| update.get("raw_input"))
        .cloned()
        .unwrap_or_else(|| json!({"title": update.get("title").cloned().unwrap_or(Value::Null)}));
    let diff = update
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| {
            content
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("diff"))
        });
    if let (Value::Object(input), Some(diff)) = (&mut input, diff) {
        for (target, camel, snake) in [
            ("file_path", "path", "path"),
            ("old_string", "oldText", "old_text"),
            ("new_string", "newText", "new_text"),
        ] {
            if let Some(value) = diff
                .get(camel)
                .or_else(|| diff.get(snake))
                .and_then(Value::as_str)
            {
                input
                    .entry(target.to_string())
                    .or_insert_with(|| Value::String(value.to_string()));
            }
        }
    }
    sanitize_value(input)
}

fn append_grok_bash_tail(update: &mut Option<GrokBashUpdate>, tail: String) {
    if tail.is_empty() {
        return;
    }
    match update {
        Some(GrokBashUpdate::Append(text) | GrokBashUpdate::Snapshot(text)) => {
            text.push_str(&tail);
        }
        None => *update = Some(GrokBashUpdate::Append(tail)),
    }
}

fn push_grok_bash_event(events: &mut Vec<SessionEvent>, call_id: &str, update: GrokBashUpdate) {
    match (call_id.is_empty(), update) {
        (_, GrokBashUpdate::Append(text)) if text.is_empty() => {}
        (true, GrokBashUpdate::Append(delta)) => {
            events.push(SessionEvent::ToolOutputDelta(delta));
        }
        (false, GrokBashUpdate::Append(delta)) => {
            events.push(SessionEvent::ToolOutputDeltaCorrelated {
                call_id: call_id.to_string(),
                delta,
            });
        }
        (true, GrokBashUpdate::Snapshot(output)) => {
            events.push(SessionEvent::ToolOutputSnapshot(output));
        }
        (false, GrokBashUpdate::Snapshot(output)) => {
            events.push(SessionEvent::ToolOutputSnapshotCorrelated {
                call_id: call_id.to_string(),
                output,
            });
        }
    }
}

fn bounded_insert(set: &mut HashSet<String>, value: String) {
    if set.len() >= 256 {
        set.clear();
    }
    set.insert(value);
}

fn bounded_tool_call_id(value: &Value) -> String {
    value
        .get("toolCallId")
        .or_else(|| value.get("tool_call_id"))
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.chars().count() <= MAX_TOOL_CALL_ID_CHARS
                && !id.chars().any(char::is_control)
        })
        .unwrap_or("")
        .to_string()
}

fn text_from_content(value: &Value) -> Option<String> {
    value
        .pointer("/content/text")
        .or_else(|| value.pointer("/content/content/text"))
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)
        .map(|text| clip_text(&redact_text(text), MAX_STREAM_DELTA_CHARS))
}

fn tool_name(value: &Value) -> String {
    if let Some(name) = value
        .get("name")
        .or_else(|| value.get("toolName"))
        .or_else(|| value.pointer("/rawInput/toolName"))
        .and_then(Value::as_str)
    {
        return clip_text(&redact_text(name), 80);
    }
    let name = match value.get("kind").and_then(Value::as_str).unwrap_or("") {
        "read" => "Read",
        "edit" => "Edit",
        "delete" => "Delete",
        "move" => "Move",
        "search" => "Grep",
        "execute" => "Bash",
        "fetch" => "WebFetch",
        "think" => "Think",
        _ => value.get("title").and_then(Value::as_str).unwrap_or("tool"),
    };
    clip_text(&redact_text(name), 80)
}

fn tool_target(input: &Value) -> String {
    for key in [
        "file_path",
        "filePath",
        "path",
        "command",
        "cmd",
        "query",
        "url",
        "target",
        "title",
    ] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            return clip_text(&redact_text(value), 180);
        }
    }
    String::new()
}

fn grok_bash_raw_output(update: &Value) -> Option<&Value> {
    let raw = update
        .get("rawOutput")
        .or_else(|| update.get("raw_output"))?;
    raw.get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bash"))
        .then_some(raw)
}

fn grok_bash_stream_delta(
    call_id: &str,
    raw: &Value,
    tools: &mut ToolState,
) -> Option<GrokBashUpdate> {
    let delta = raw.get("output_delta").or_else(|| raw.get("outputDelta"));
    if !call_id.is_empty()
        && !tools.bash_streams.contains_key(call_id)
        && tools.bash_streams.len() >= MAX_BASH_STREAMS
    {
        tools.bash_streams.clear();
    }
    let mut local_stream = GrokBashStream::default();
    let stream = if call_id.is_empty() {
        &mut local_stream
    } else {
        tools.bash_streams.entry(call_id.to_string()).or_default()
    };
    match delta {
        Some(Value::Array(values)) => {
            let bytes = json_byte_array(values)?;
            if bytes.is_empty() {
                stream.reset();
                Some(GrokBashUpdate::Snapshot(String::new()))
            } else {
                Some(GrokBashUpdate::Append(redact_text(&stream.append(&bytes))))
            }
        }
        Some(Value::Null) | None => {
            let bytes = json_byte_array(raw.get("output")?.as_array()?)?;
            Some(GrokBashUpdate::Snapshot(redact_text(
                &stream.replace(&bytes),
            )))
        }
        Some(_) => None,
    }
}

fn json_byte_array(values: &[Value]) -> Option<Vec<u8>> {
    values
        .iter()
        .map(|byte| u8::try_from(byte.as_u64()?).ok())
        .collect()
}

fn grok_bash_terminal_summary(raw: &Value) -> String {
    if let Some(summary) = raw
        .get("output_for_prompt")
        .or_else(|| raw.get("outputForPrompt"))
        .and_then(Value::as_str)
        .filter(|summary| !summary.trim().is_empty())
    {
        let mut sanitizer = TerminalTextSanitizer::new();
        let mut safe = sanitizer.push(summary.as_bytes());
        safe.push_str(&sanitizer.finish());
        return clip_text(&redact_text(&safe), MAX_DIAGNOSTIC_CHARS);
    }

    if raw
        .get("timed_out")
        .or_else(|| raw.get("timedOut"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "Command timed out".to_string();
    }
    if let Some(signal) = raw.get("signal").and_then(Value::as_str) {
        return format!("Command stopped by {}", clip_text(&redact_text(signal), 80));
    }
    if let Some(code) = raw
        .get("exit_code")
        .or_else(|| raw.get("exitCode"))
        .and_then(Value::as_i64)
    {
        return format!("Command exited with code {code}");
    }
    String::new()
}

fn tool_output_summary(update: &Value) -> String {
    if let Some(raw) = update.get("rawOutput").or_else(|| update.get("raw_output")) {
        return safe_value_summary(raw);
    }
    let Some(content) = update.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    let text = content
        .iter()
        .filter_map(|item| {
            item.pointer("/content/text")
                .or_else(|| item.pointer("/content/content/text"))
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n");
    clip_text(&redact_text(&text), MAX_TOOL_RESULT_CHARS)
}

fn safe_value_summary(value: &Value) -> String {
    if contains_sensitive_key(value) {
        return "[redacted sensitive tool output]".to_string();
    }
    match value {
        Value::String(text) => clip_text(&redact_text(text), MAX_TOOL_RESULT_CHARS),
        Value::Null => String::new(),
        _ => clip_text(&value.to_string(), MAX_TOOL_RESULT_CHARS),
    }
}

fn parse_prompt_result(result: &Value) -> (TurnStatus, Option<Usage>) {
    let reason = result
        .get("stopReason")
        .or_else(|| result.get("stop_reason"))
        .and_then(Value::as_str);
    let status = match reason {
        None => TurnStatus::Failed("ACP prompt response omitted stopReason".to_string()),
        Some("") => TurnStatus::Failed("ACP prompt response used an empty stopReason".to_string()),
        Some("end_turn") => TurnStatus::Completed,
        Some("max_tokens" | "max_turn_requests") => TurnStatus::Truncated,
        Some("cancelled" | "canceled") => TurnStatus::Interrupted,
        Some("refusal") => TurnStatus::Failed("ACP agent refused the prompt".to_string()),
        Some(other) => TurnStatus::Failed(format!("ACP turn stopped: {}", clip_text(other, 80))),
    };
    // Grok's pinned wire contract is intentionally asymmetric:
    // `_meta.usage` is the whole-prompt ledger, while sibling `_meta` token
    // fields describe only the LAST model call. Presence of a null/malformed
    // `usage` therefore fails closed. Falling through to siblings would turn a
    // partial sample into a fabricated turn total.
    let usage = result
        .get("_meta")
        .and_then(|meta| meta.get("usage"))
        .and_then(parse_usage);
    (status, usage)
}

fn grok_turn_status(stop_reason: &str, agent_result: Option<&str>) -> TurnStatus {
    match stop_reason {
        "end_turn" => TurnStatus::Completed,
        "max_tokens" | "max_turn_requests" => TurnStatus::Truncated,
        "cancelled" | "canceled" => TurnStatus::Interrupted,
        "refusal" => TurnStatus::Failed("ACP agent refused the prompt".to_string()),
        "error" => {
            let detail = agent_result
                .map(redact_text)
                .filter(|value| !value.trim().is_empty())
                .map_or_else(
                    || "Grok Build turn failed".to_string(),
                    |value| clip_text(&value, MAX_DIAGNOSTIC_CHARS),
                );
            TurnStatus::Failed(detail)
        }
        other => TurnStatus::Failed(format!("ACP turn stopped: {}", clip_text(other, 80))),
    }
}

fn usage_from_event(params: &Value) -> Option<Usage> {
    let update = params.get("update").unwrap_or(params);
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str);
    match kind {
        // The durable Grok rail carries the same whole-prompt PromptUsage shape.
        // Generic/legacy `usage_update` frames are deliberately not accepted:
        // their scope is unspecified and may be a last-call sample.
        Some("turn_completed") => update.get("usage").and_then(parse_usage),
        _ => None,
    }
}

fn parse_usage(value: &Value) -> Option<Usage> {
    value.as_object()?;
    let u64_field = |camel: &str, snake: &str| -> Option<u64> {
        value.get(camel).or_else(|| value.get(snake))?.as_u64()
    };
    let bool_field = |camel: &str, snake: &str| -> Option<bool> {
        match value.get(camel).or_else(|| value.get(snake)) {
            None => Some(false),
            Some(field) => field.as_bool(),
        }
    };

    let input_tokens = u64_field("inputTokens", "input_tokens")?;
    let output_tokens = u64_field("outputTokens", "output_tokens")?;
    let total_tokens = u64_field("totalTokens", "total_tokens")?;
    let cached_read_tokens = u64_field("cachedReadTokens", "cached_read_tokens")?;
    let reasoning_tokens = u64_field("reasoningTokens", "reasoning_tokens")?;
    let model_calls = u64_field("modelCalls", "model_calls")?;
    let num_turns = u64_field("numTurns", "num_turns")?;
    let usage_incomplete = bool_field("usageIsIncomplete", "usage_is_incomplete")?;
    let cost_partial = bool_field("costIsPartial", "cost_is_partial")?;

    // These identities are guaranteed by Grok's UsageLedger. Rejecting a
    // contradictory frame is safer than displaying or persisting a total that
    // cannot be true. Cache/reasoning are subsets, not additional tokens.
    if total_tokens != input_tokens.saturating_add(output_tokens)
        || cached_read_tokens > input_tokens
        || reasoning_tokens > output_tokens
        || num_turns > model_calls
    {
        return None;
    }

    let mut cost_usd_ticks = match value
        .get("costUsdTicks")
        .or_else(|| value.get("cost_usd_ticks"))
    {
        None | Some(Value::Null) => None,
        Some(field) => match field.as_i64()? {
            ticks if ticks > 0 => Some(ticks),
            _ => None,
        },
    };
    // Official Grok output already scrubs these ticks. Repeat the rule at the
    // trust boundary so a future or malicious peer cannot present a partial bill
    // as exact merely by retaining the numeric field.
    if usage_incomplete || cost_partial {
        cost_usd_ticks = None;
    }

    Some(Usage {
        input_tokens,
        output_tokens,
        total_tokens,
        cached_read_tokens,
        cached_write_tokens: 0,
        reasoning_tokens,
        model_calls,
        num_turns,
        cost_usd_ticks,
        usage_incomplete,
        cost_partial,
        scope: umadev_runtime::UsageScope::WholePrompt,
    })
}

fn extract_session_model(value: &Value) -> Option<String> {
    for pointer in [
        "/model",
        "/modelId",
        "/currentModelId",
        "/models/currentModelId",
        "/_meta/model",
        "/_meta/modelId",
    ] {
        if let Some(model) = value.pointer(pointer).and_then(Value::as_str) {
            if !model.trim().is_empty() {
                return Some(clip_text(model, 160));
            }
        }
    }
    value
        .get("configOptions")
        .or_else(|| value.get("config_options"))
        .and_then(Value::as_array)
        .and_then(|options| {
            options.iter().find_map(|option| {
                let id = option.get("id").and_then(Value::as_str).unwrap_or("");
                let normalized = id.to_ascii_lowercase();
                let has_model_suffix = normalized
                    .rsplit_once('.')
                    .is_some_and(|(_, suffix)| suffix == "model");
                if normalized == "model" || has_model_suffix {
                    option
                        .get("currentValue")
                        .or_else(|| option.get("value"))
                        .and_then(Value::as_str)
                        .filter(|model| !model.trim().is_empty())
                        .map(|model| clip_text(model, 160))
                } else {
                    None
                }
            })
        })
}

fn parse_approval_options(params: &Value) -> Vec<HostApprovalOption> {
    let Some(options) = params.get("options").and_then(Value::as_array) else {
        return Vec::new();
    };
    options
        .iter()
        .filter_map(|option| {
            let id = option
                .get("optionId")
                .or_else(|| option.get("option_id"))
                .and_then(Value::as_str)?;
            if id.is_empty() {
                return None;
            }
            let raw_kind = option.get("kind").and_then(Value::as_str).unwrap_or("");
            let kind = match raw_kind {
                "allow_once" => HostApprovalOptionKind::AllowOnce,
                "allow_always" => HostApprovalOptionKind::AllowAlways,
                "reject_once" => HostApprovalOptionKind::RejectOnce,
                "reject_always" => HostApprovalOptionKind::RejectAlways,
                other => HostApprovalOptionKind::Other(other.to_string()),
            };
            Some(HostApprovalOption {
                id: id.to_string(),
                label: option
                    .get("name")
                    .or_else(|| option.get("label"))
                    .and_then(Value::as_str)
                    .map_or_else(|| id.to_string(), |text| clip_text(text, 160)),
                kind,
            })
        })
        .collect()
}

/// Parse the exact canonical three-button permission surface from the audited
/// Kimi adapter. Option order, ids, labels, and semantic kinds are all
/// load-bearing in upstream; accepting a widened or reordered set would make
/// UmaDev guess which opaque id grants authority.
fn kimi_ordinary_permission_options(params: &Value) -> Option<Vec<HostApprovalOption>> {
    let options = params.get("options")?.as_array()?;
    let expected = [
        ("approve_once", "Approve once", "allow_once"),
        ("approve_always", "Approve for this session", "allow_always"),
        ("reject", "Reject", "reject_once"),
    ];
    if options.len() != expected.len()
        || !options
            .iter()
            .zip(expected)
            .all(|(option, (id, name, kind))| {
                option.get("optionId").and_then(Value::as_str) == Some(id)
                    && option.get("name").and_then(Value::as_str) == Some(name)
                    && option.get("kind").and_then(Value::as_str) == Some(kind)
            })
    {
        return None;
    }
    Some(parse_approval_options(params))
}

fn validate_grok_ask_user_question(params: &Value) -> Result<(), &'static str> {
    let session_ok = bounded_grok_field(params.get("sessionId"), MAX_GROK_ID_CHARS, false);
    let tool_ok = bounded_grok_field(params.get("toolCallId"), MAX_GROK_ID_CHARS, false);
    let mode_ok = params
        .get("mode")
        .and_then(Value::as_str)
        .is_some_and(|mode| matches!(mode, "default" | "plan"));
    if !session_ok || !tool_ok || !mode_ok {
        return Err("invalid Grok ask_user_question correlation fields");
    }
    let Some(questions) = params.get("questions").and_then(Value::as_array) else {
        return Err("invalid Grok ask_user_question questions");
    };
    if questions.is_empty() || questions.len() > MAX_GROK_QUESTIONS {
        return Err("Grok ask_user_question contained no questions");
    }
    let mut prompts = HashSet::new();
    for question in questions {
        let Some(prompt) = question
            .get("question")
            .and_then(Value::as_str)
            .filter(|value| bounded_grok_text(value, MAX_GROK_QUESTION_CHARS, false))
        else {
            return Err("invalid Grok ask_user_question question");
        };
        if !prompts.insert(prompt) {
            return Err("Grok ask_user_question repeated a question");
        }
        if question.get("id").is_some_and(|value| {
            !value.is_null() && !bounded_grok_field(Some(value), MAX_GROK_ID_CHARS, false)
        }) || question
            .get("multiSelect")
            .is_some_and(|value| !value.is_boolean() && !value.is_null())
        {
            return Err("invalid Grok ask_user_question question fields");
        }
        let Some(options) = question.get("options").and_then(Value::as_array) else {
            return Err("invalid Grok ask_user_question options");
        };
        if options.len() > MAX_GROK_OPTIONS_PER_QUESTION {
            return Err("invalid Grok ask_user_question options");
        }
        for option in options {
            if !bounded_grok_field(option.get("label"), MAX_GROK_OPTION_LABEL_CHARS, false)
                || !bounded_grok_field(
                    option.get("description"),
                    MAX_GROK_OPTION_DESCRIPTION_CHARS,
                    true,
                )
                || option.get("preview").is_some_and(|value| {
                    !value.is_null()
                        && !bounded_grok_field(Some(value), MAX_GROK_PREVIEW_CHARS, true)
                })
                || option.get("id").is_some_and(|value| {
                    !value.is_null() && !bounded_grok_field(Some(value), MAX_GROK_ID_CHARS, false)
                })
            {
                return Err("invalid Grok ask_user_question option");
            }
        }
    }
    Ok(())
}

fn validate_grok_exit_plan_mode(params: &Value) -> Result<(), &'static str> {
    let session_ok = bounded_grok_field(params.get("sessionId"), MAX_GROK_ID_CHARS, false);
    let tool_ok = bounded_grok_field(params.get("toolCallId"), MAX_GROK_ID_CHARS, false);
    let plan_ok = params.get("planContent").is_some_and(|value| {
        value.is_null() || bounded_grok_field(Some(value), MAX_GROK_PLAN_CHARS, true)
    });
    if session_ok && tool_ok && plan_ok {
        Ok(())
    } else {
        Err("invalid Grok exit_plan_mode params")
    }
}

fn bounded_grok_field(value: Option<&Value>, max_chars: usize, allow_empty: bool) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| bounded_grok_text(text, max_chars, allow_empty))
}

fn bounded_grok_text(text: &str, max_chars: usize, allow_empty: bool) -> bool {
    (allow_empty || !text.trim().is_empty())
        && text.chars().take(max_chars + 1).count() <= max_chars
        && !text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
}

fn parse_host_questions(params: &Value) -> Vec<HostQuestion> {
    let values = params
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| vec![params.clone()]);
    values
        .iter()
        .enumerate()
        .filter_map(|(index, question)| {
            let prompt = question
                .get("question")
                .or_else(|| question.get("prompt"))
                .or_else(|| question.get("message"))
                .and_then(Value::as_str)?;
            if prompt.trim().is_empty() {
                return None;
            }
            let options = question
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(parse_host_question_option)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let kind = if question
                .get("multiSelect")
                .or_else(|| question.get("multi_select"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                HostQuestionKind::MultiChoice
            } else if !options.is_empty() {
                HostQuestionKind::SingleChoice
            } else {
                match question
                    .get("kind")
                    .or_else(|| question.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                {
                    "secret" | "password" => HostQuestionKind::Secret,
                    "confirmation" | "confirm" | "boolean" => HostQuestionKind::Confirmation,
                    "text" | "input" => HostQuestionKind::Text,
                    other => HostQuestionKind::Other(other.to_string()),
                }
            };
            Some(HostQuestion {
                id: question
                    .get("id")
                    .or_else(|| question.get("questionId"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map_or_else(|| format!("q{}", index + 1), str::to_string),
                header: question
                    .get("header")
                    .or_else(|| question.get("title"))
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, 160)),
                prompt: clip_text(prompt, 4096),
                kind,
                required: question
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                options,
            })
        })
        .collect()
}

fn parse_host_question_option(option: &Value) -> Option<HostQuestionOption> {
    match option {
        Value::String(text) if !text.is_empty() => Some(HostQuestionOption {
            value: text.clone(),
            label: text.clone(),
            description: None,
            preview: None,
        }),
        Value::Object(_) => {
            let value = option
                .get("value")
                .or_else(|| option.get("id"))
                .or_else(|| option.get("label"))
                .and_then(Value::as_str)?;
            Some(HostQuestionOption {
                value: value.to_string(),
                label: option
                    .get("label")
                    .or_else(|| option.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_string(),
                description: option
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, MAX_GROK_OPTION_DESCRIPTION_CHARS)),
                preview: option
                    .get("preview")
                    .and_then(Value::as_str)
                    .map(|text| clip_text(text, MAX_GROK_PREVIEW_CHARS)),
            })
        }
        _ => None,
    }
}

fn parse_host_permissions(params: &Value) -> Vec<HostPermission> {
    params
        .get("permissions")
        .or_else(|| params.get("scopes"))
        .and_then(Value::as_array)
        .map(|permissions| {
            permissions
                .iter()
                .filter_map(|permission| match permission {
                    Value::String(kind) => Some(HostPermission {
                        kind: clip_text(kind, 120),
                        target: None,
                        metadata: Value::Null,
                    }),
                    Value::Object(_) => Some(HostPermission {
                        kind: permission
                            .get("kind")
                            .or_else(|| permission.get("type"))
                            .and_then(Value::as_str)
                            .map_or_else(|| "unknown".to_string(), |text| clip_text(text, 120)),
                        target: permission
                            .get("target")
                            .or_else(|| permission.get("path"))
                            .or_else(|| permission.get("host"))
                            .and_then(Value::as_str)
                            .map(|text| clip_text(text, 512)),
                        metadata: Value::Null,
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn write_permission_response(
    writer: &SharedWriter,
    raw_id: &Value,
    option_id: Option<&str>,
    followup_message: Option<&str>,
) -> Result<(), SessionError> {
    write_json_line(
        writer,
        &permission_response_frame(raw_id, option_id, followup_message),
    )
    .await
}

fn permission_response_frame(
    raw_id: &Value,
    option_id: Option<&str>,
    followup_message: Option<&str>,
) -> Value {
    let outcome = option_id.map_or_else(
        || json!({"outcome":"cancelled"}),
        |option_id| json!({"outcome":"selected", "optionId":option_id}),
    );
    let mut result = json!({"outcome":outcome});
    if let Some(message) = followup_message
        .map(redact_text)
        .map(|message| clip_text(&message, MAX_PERMISSION_FOLLOWUP_CHARS))
        .filter(|message| !message.trim().is_empty())
    {
        result["_meta"] = json!({"followup_message":message});
    }
    json!({"jsonrpc":"2.0", "id":raw_id, "result":result})
}

fn folder_trust_response_frame(raw_id: &Value, decision: FolderTrustUserDecision) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":raw_id,
        "result":decision.to_wire_value()
    })
}

async fn write_host_response(
    writer: &SharedWriter,
    pending: PendingHostRequest,
    response: HostResponse,
) -> Result<(), SessionError> {
    match (pending, response) {
        (
            PendingHostRequest::Approval(record),
            HostResponse::Approval {
                decision,
                selected_option_id,
                message,
            },
        ) => {
            write_approval_host_response(writer, &record, decision, selected_option_id, message)
                .await
        }
        (
            PendingHostRequest::UserInput {
                raw_id,
                questions,
                flavor: UserInputFlavor::Generic,
            },
            HostResponse::UserInput { answers },
        ) => write_generic_user_input_response(writer, raw_id, &questions, &answers).await,
        (
            PendingHostRequest::UserInput {
                raw_id,
                questions,
                flavor: UserInputFlavor::GrokAskUserQuestion,
            },
            HostResponse::UserInputOutcome { outcome },
        ) => write_grok_user_input_outcome(writer, raw_id, &questions, &outcome).await,
        (
            PendingHostRequest::UserInput {
                raw_id,
                questions,
                flavor: UserInputFlavor::KimiPermissionQuestion,
            },
            HostResponse::UserInput { answers },
        ) => write_kimi_permission_question_response(writer, raw_id, &questions, &answers).await,
        (
            PendingHostRequest::UserInput {
                raw_id,
                questions,
                flavor: UserInputFlavor::KimiPlanReview,
            },
            HostResponse::UserInput { answers },
        ) => write_kimi_plan_review_response(writer, raw_id, &questions, &answers).await,
        (
            PendingHostRequest::PermissionExpansion { raw_id },
            HostResponse::PermissionExpansion {
                decision,
                granted,
                message,
            },
        ) => {
            write_permission_expansion_host_response(
                writer,
                raw_id,
                decision,
                &granted,
                message.as_deref(),
            )
            .await
        }
        (
            PendingHostRequest::McpElicitation { raw_id },
            HostResponse::McpElicitation { action, content },
        ) => write_mcp_elicitation_host_response(writer, raw_id, action, content).await,
        (
            PendingHostRequest::PlanConfirmation {
                raw_id,
                flavor: PlanConfirmationFlavor::Generic,
            },
            HostResponse::PlanConfirmation { decision, feedback },
        ) => {
            write_json_line(
                writer,
                &plan_confirmation_response_frame(&raw_id, decision, feedback),
            )
            .await
        }
        (
            PendingHostRequest::PlanConfirmation {
                raw_id,
                flavor: PlanConfirmationFlavor::GrokExitPlanMode,
            },
            HostResponse::PlanOutcome { outcome },
        ) => write_grok_plan_outcome(writer, raw_id, &outcome).await,
        (
            PendingHostRequest::FolderTrust { raw_id, .. },
            HostResponse::FolderTrust { decision },
        ) => write_folder_trust_host_response(writer, raw_id, decision).await,
        (pending, HostResponse::Cancelled { reason }) => {
            write_cancelled_host_response(writer, pending, reason).await
        }
        (pending, _) => reject_mismatched_host_response(writer, pending).await,
    }
}

async fn write_mcp_elicitation_host_response(
    writer: &SharedWriter,
    raw_id: Value,
    action: HostElicitationAction,
    content: Option<Value>,
) -> Result<(), SessionError> {
    write_json_line(
        writer,
        &mcp_elicitation_response_frame(&raw_id, action, content.as_ref()),
    )
    .await
}

async fn write_folder_trust_host_response(
    writer: &SharedWriter,
    raw_id: Value,
    decision: HostFolderTrustDecision,
) -> Result<(), SessionError> {
    let decision = match decision {
        HostFolderTrustDecision::Trust => FolderTrustUserDecision::Trust,
        HostFolderTrustDecision::KeepGated => FolderTrustUserDecision::KeepGated,
    };
    write_json_line(writer, &folder_trust_response_frame(&raw_id, decision)).await
}

async fn write_permission_expansion_host_response(
    writer: &SharedWriter,
    raw_id: Value,
    decision: ApprovalDecision,
    granted: &[HostPermission],
    message: Option<&str>,
) -> Result<(), SessionError> {
    write_json_line(
        writer,
        &permission_expansion_response_frame(&raw_id, decision, granted, message),
    )
    .await
}

async fn write_generic_user_input_response(
    writer: &SharedWriter,
    raw_id: Value,
    questions: &[PendingQuestion],
    answers: &[HostAnswer],
) -> Result<(), SessionError> {
    write_user_input_response(writer, raw_id, questions, UserInputFlavor::Generic, answers).await
}

async fn write_approval_host_response(
    writer: &SharedWriter,
    record: &ApprovalRecord,
    decision: ApprovalDecision,
    selected_option_id: Option<String>,
    message: Option<String>,
) -> Result<(), SessionError> {
    let selected = selected_option_id
        .filter(|id| record.validates_selected(id, decision))
        .or_else(|| record.option_for_decision(decision));
    write_permission_response(
        writer,
        &record.raw_id,
        selected.as_deref(),
        message.as_deref(),
    )
    .await
}

async fn write_grok_user_input_outcome(
    writer: &SharedWriter,
    raw_id: Value,
    questions: &[PendingQuestion],
    outcome: &HostUserInputOutcome,
) -> Result<(), SessionError> {
    let result = match grok_user_input_outcome_result(questions, outcome) {
        Ok(result) => result,
        Err(message) => {
            reply_rpc_error(writer, raw_id, -32_602, message).await;
            return Ok(());
        }
    };
    write_json_line(
        writer,
        &json!({"jsonrpc":"2.0", "id":raw_id, "result":result}),
    )
    .await
}

async fn write_kimi_permission_question_response(
    writer: &SharedWriter,
    raw_id: Value,
    questions: &[PendingQuestion],
    answers: &[HostAnswer],
) -> Result<(), SessionError> {
    let selected = questions.first().and_then(|question| {
        answers
            .iter()
            .find(|answer| answer.question_id == question.id)
            .and_then(|answer| answer.values.first())
            .filter(|value| question.option_labels.contains_key(value.as_str()))
    });
    write_json_line(
        writer,
        &permission_response_frame(&raw_id, selected.map(String::as_str), None),
    )
    .await
}

async fn write_kimi_plan_review_response(
    writer: &SharedWriter,
    raw_id: Value,
    questions: &[PendingQuestion],
    answers: &[HostAnswer],
) -> Result<(), SessionError> {
    let selected = match (questions, answers) {
        ([question], [answer])
            if answer.question_id == question.id
                && answer.values.len() == 1
                && question
                    .option_labels
                    .contains_key(answer.values[0].as_str()) =>
        {
            Some(answer.values[0].as_str())
        }
        _ => None,
    };
    write_permission_response(writer, &raw_id, selected, None).await
}

fn permission_expansion_response_frame(
    raw_id: &Value,
    decision: ApprovalDecision,
    granted: &[HostPermission],
    message: Option<&str>,
) -> Value {
    json!({
        "jsonrpc":"2.0", "id":raw_id,
        "result":{
            "approved":matches!(decision, ApprovalDecision::Allow),
            "granted":granted,
            "message":message
        }
    })
}

fn mcp_elicitation_response_frame(
    raw_id: &Value,
    action: HostElicitationAction,
    content: Option<&Value>,
) -> Value {
    let action = match action {
        HostElicitationAction::Accept => "accept",
        HostElicitationAction::Decline => "decline",
        HostElicitationAction::Cancel => "cancel",
    };
    json!({
        "jsonrpc":"2.0", "id":raw_id,
        "result":{"action":action, "content":content}
    })
}

fn plan_confirmation_response_frame(
    raw_id: &Value,
    decision: ApprovalDecision,
    feedback: Option<String>,
) -> Value {
    let result = plan_confirmation_result(PlanConfirmationFlavor::Generic, decision, feedback);
    json!({"jsonrpc":"2.0", "id":raw_id, "result":result})
}

async fn write_grok_plan_outcome(
    writer: &SharedWriter,
    raw_id: Value,
    outcome: &HostPlanOutcome,
) -> Result<(), SessionError> {
    let result = match grok_plan_outcome_result(outcome) {
        Ok(result) => result,
        Err(message) => {
            reply_rpc_error(writer, raw_id, -32_602, message).await;
            return Ok(());
        }
    };
    write_json_line(
        writer,
        &json!({"jsonrpc":"2.0", "id":raw_id, "result":result}),
    )
    .await
}

async fn write_cancelled_host_response(
    writer: &SharedWriter,
    pending: PendingHostRequest,
    reason: Option<String>,
) -> Result<(), SessionError> {
    write_json_line(writer, &cancelled_host_response_frame(pending, reason)).await
}

fn cancelled_host_response_frame(pending: PendingHostRequest, reason: Option<String>) -> Value {
    match pending {
        PendingHostRequest::Approval(record) => {
            permission_response_frame(&record.raw_id, None, None)
        }
        PendingHostRequest::PermissionExpansion { raw_id } => json!({
            "jsonrpc":"2.0", "id":raw_id,
            "result":{"approved":false, "granted":[], "message":reason}
        }),
        PendingHostRequest::McpElicitation { raw_id } => json!({
            "jsonrpc":"2.0", "id":raw_id, "result":{"action":"cancel"}
        }),
        PendingHostRequest::PlanConfirmation { raw_id, flavor } => match flavor {
            PlanConfirmationFlavor::GrokExitPlanMode => json!({
                "jsonrpc":"2.0", "id":raw_id,
                "result":grok_plan_response("cancelled", reason)
            }),
            PlanConfirmationFlavor::Generic => json!({
                "jsonrpc":"2.0", "id":raw_id,
                "result":{"approved":false, "feedback":reason}
            }),
        },
        PendingHostRequest::FolderTrust { raw_id, .. } => {
            folder_trust_response_frame(&raw_id, FolderTrustUserDecision::KeepGated)
        }
        PendingHostRequest::UserInput { raw_id, flavor, .. } => match flavor {
            UserInputFlavor::GrokAskUserQuestion => json!({
                "jsonrpc":"2.0", "id":raw_id,
                "result":{"outcome":"cancelled"}
            }),
            UserInputFlavor::KimiPermissionQuestion => {
                permission_response_frame(&raw_id, None, None)
            }
            UserInputFlavor::KimiPlanReview => permission_response_frame(&raw_id, None, None),
            UserInputFlavor::Generic => json!({
                "jsonrpc":"2.0", "id":raw_id,
                "error":{"code":-32_000, "message":reason.unwrap_or_else(|| "host interaction cancelled".to_string())}
            }),
        },
    }
}

fn grok_user_input_result(questions: &[PendingQuestion], answers: &[HostAnswer]) -> Value {
    grok_accepted_result(questions, answers, &[]).unwrap_or_else(|_| json!({"outcome":"cancelled"}))
}

fn grok_user_input_outcome_result(
    questions: &[PendingQuestion],
    outcome: &HostUserInputOutcome,
) -> Result<Value, &'static str> {
    match outcome {
        HostUserInputOutcome::Accepted {
            answers,
            annotations,
        } => grok_accepted_result(questions, answers, annotations),
        HostUserInputOutcome::ChatAboutThis { partial_answers } => Ok(json!({
            "outcome":"chat_about_this",
            "partial_answers":grok_partial_answers(questions, partial_answers)?
        })),
        HostUserInputOutcome::SkipInterview { partial_answers } => Ok(json!({
            "outcome":"skip_interview",
            "partial_answers":grok_partial_answers(questions, partial_answers)?
        })),
        HostUserInputOutcome::Cancelled => Ok(json!({"outcome":"cancelled"})),
    }
}

fn grok_accepted_result(
    questions: &[PendingQuestion],
    answers: &[HostAnswer],
    annotations: &[HostQuestionAnnotation],
) -> Result<Value, &'static str> {
    let answers_by_id = index_grok_answers(questions, answers)?;
    let annotations_by_id = index_grok_annotations(questions, annotations)?;
    let mut wire_answers = serde_json::Map::new();
    let mut wire_annotations = serde_json::Map::new();
    for question in questions {
        let Some(answer) = answers_by_id.get(question.id.as_str()).copied() else {
            continue;
        };
        let (mut labels, freeform, selected_preview) = resolve_grok_answer(question, answer)?;
        let annotation = annotations_by_id.get(question.id.as_str()).copied();
        let explicit_notes = annotation
            .and_then(|annotation| annotation.notes.as_deref())
            .map(str::trim)
            .filter(|notes| !notes.is_empty());
        let notes = explicit_notes
            .map(str::to_string)
            .or_else(|| (!freeform.is_empty()).then(|| freeform.join("\n")));
        if labels.is_empty() && notes.is_some() {
            labels.push("Other".to_string());
        }
        if labels.is_empty() {
            continue;
        }
        wire_answers.insert(question.prompt.clone(), json!(labels));

        let supplied_preview = annotation.and_then(|annotation| annotation.preview.as_deref());
        if supplied_preview.is_some_and(|preview| Some(preview) != selected_preview.as_deref()) {
            return Err("Grok question annotation preview did not match the selected option");
        }
        if let Some(notes) = notes.as_deref() {
            if !bounded_grok_text(notes, MAX_GROK_NOTES_CHARS, true) {
                return Err("Grok question notes exceeded the supported bound");
            }
        }
        if selected_preview.is_some() || notes.is_some() {
            let mut annotation = serde_json::Map::new();
            if let Some(preview) = selected_preview {
                annotation.insert("preview".to_string(), Value::String(preview));
            }
            if let Some(notes) = notes {
                annotation.insert("notes".to_string(), Value::String(notes));
            }
            wire_annotations.insert(question.prompt.clone(), Value::Object(annotation));
        }
    }
    let mut result = json!({"outcome":"accepted", "answers":wire_answers});
    if !wire_annotations.is_empty() {
        result["annotations"] = Value::Object(wire_annotations);
    }
    Ok(result)
}

fn index_grok_answers<'a>(
    questions: &[PendingQuestion],
    answers: &'a [HostAnswer],
) -> Result<HashMap<&'a str, &'a HostAnswer>, &'static str> {
    if answers.len() > questions.len() {
        return Err("Grok response contained too many answers");
    }
    let valid_ids = questions
        .iter()
        .map(|question| question.id.as_str())
        .collect::<HashSet<_>>();
    let mut indexed = HashMap::with_capacity(answers.len());
    for answer in answers {
        if !valid_ids.contains(answer.question_id.as_str())
            || indexed
                .insert(answer.question_id.as_str(), answer)
                .is_some()
        {
            return Err("Grok response used an unknown or repeated question id");
        }
    }
    Ok(indexed)
}

fn index_grok_annotations<'a>(
    questions: &[PendingQuestion],
    annotations: &'a [HostQuestionAnnotation],
) -> Result<HashMap<&'a str, &'a HostQuestionAnnotation>, &'static str> {
    if annotations.len() > questions.len() {
        return Err("Grok response contained too many annotations");
    }
    let valid_ids = questions
        .iter()
        .map(|question| question.id.as_str())
        .collect::<HashSet<_>>();
    let mut indexed = HashMap::with_capacity(annotations.len());
    for annotation in annotations {
        if !valid_ids.contains(annotation.question_id.as_str())
            || indexed
                .insert(annotation.question_id.as_str(), annotation)
                .is_some()
        {
            return Err("Grok response used an unknown or repeated annotation id");
        }
        if annotation
            .notes
            .as_deref()
            .is_some_and(|notes| !bounded_grok_text(notes, MAX_GROK_NOTES_CHARS, true))
            || annotation
                .preview
                .as_deref()
                .is_some_and(|preview| !bounded_grok_text(preview, MAX_GROK_PREVIEW_CHARS, true))
        {
            return Err("Grok response annotation exceeded the supported bound");
        }
    }
    Ok(indexed)
}

type GrokResolvedAnswer = (Vec<String>, Vec<String>, Option<String>);

fn resolve_grok_answer(
    question: &PendingQuestion,
    answer: &HostAnswer,
) -> Result<GrokResolvedAnswer, &'static str> {
    if answer.values.len() > MAX_GROK_OPTIONS_PER_QUESTION + 1 {
        return Err("Grok response contained too many selected values");
    }
    let mut labels = Vec::new();
    let mut freeform = Vec::new();
    let mut preview = None;
    for value in &answer.values {
        if !bounded_grok_text(value, MAX_GROK_NOTES_CHARS, true) {
            return Err("Grok response value exceeded the supported bound");
        }
        if let Some(label) = question.option_labels.get(value) {
            if !labels.contains(label) {
                labels.push(label.clone());
            }
            if !question.multi_select && preview.is_none() {
                preview = question.option_previews.get(value).cloned();
            }
        } else if !value.trim().is_empty() {
            freeform.push(value.trim().to_string());
        }
    }
    Ok((labels, freeform, preview))
}

fn grok_partial_answers(
    questions: &[PendingQuestion],
    answers: &[HostAnswer],
) -> Result<Value, &'static str> {
    let answers_by_id = index_grok_answers(questions, answers)?;
    let mut partial = serde_json::Map::new();
    for question in questions {
        let Some(answer) = answers_by_id.get(question.id.as_str()).copied() else {
            continue;
        };
        let (mut labels, freeform, _) = resolve_grok_answer(question, answer)?;
        if labels.is_empty() && !freeform.is_empty() {
            labels.push("Other".to_string());
        }
        if !labels.is_empty() {
            partial.insert(question.prompt.clone(), Value::String(labels.join(", ")));
        }
    }
    Ok(Value::Object(partial))
}

fn plan_confirmation_result(
    flavor: PlanConfirmationFlavor,
    decision: ApprovalDecision,
    feedback: Option<String>,
) -> Value {
    match flavor {
        PlanConfirmationFlavor::GrokExitPlanMode => grok_plan_response(
            if matches!(decision, ApprovalDecision::Allow) {
                "approved"
            } else {
                "cancelled"
            },
            feedback,
        ),
        PlanConfirmationFlavor::Generic => json!({
            "approved":matches!(decision, ApprovalDecision::Allow),
            "feedback":feedback
        }),
    }
}

fn grok_plan_response(outcome: &str, feedback: Option<String>) -> Value {
    let mut result = json!({"outcome":outcome});
    if let Some(feedback) = feedback.filter(|value| !value.trim().is_empty()) {
        result["feedback"] = Value::String(feedback.trim().to_string());
    }
    result
}

fn grok_plan_outcome_result(outcome: &HostPlanOutcome) -> Result<Value, &'static str> {
    match outcome {
        HostPlanOutcome::Approved => Ok(json!({"outcome":"approved"})),
        HostPlanOutcome::Abandoned => Ok(json!({"outcome":"abandoned"})),
        HostPlanOutcome::Cancelled { feedback } => {
            if feedback
                .as_deref()
                .is_some_and(|feedback| !bounded_grok_text(feedback, MAX_GROK_NOTES_CHARS, true))
            {
                return Err("Grok plan feedback exceeded the supported bound");
            }
            Ok(grok_plan_response("cancelled", feedback.clone()))
        }
    }
}

async fn write_user_input_response(
    writer: &SharedWriter,
    raw_id: Value,
    questions: &[PendingQuestion],
    flavor: UserInputFlavor,
    answers: &[HostAnswer],
) -> Result<(), SessionError> {
    let result = match flavor {
        UserInputFlavor::Generic => {
            let valid_ids = questions
                .iter()
                .map(|question| &question.id)
                .collect::<HashSet<_>>();
            let answers = answers
                .iter()
                .filter(|answer| valid_ids.contains(&answer.question_id))
                .collect::<Vec<_>>();
            json!({"answers":answers})
        }
        UserInputFlavor::GrokAskUserQuestion => grok_user_input_result(questions, answers),
        UserInputFlavor::KimiPermissionQuestion => {
            // Routed through `write_kimi_permission_question_response`; keep
            // this shared helper exhaustive without inventing a second wire shape.
            json!({"outcome":"cancelled"})
        }
        UserInputFlavor::KimiPlanReview => {
            // Routed through `write_kimi_plan_review_response`; an unknown
            // generic route must never pick a plan variant.
            json!({"outcome":"cancelled"})
        }
    };
    write_json_line(
        writer,
        &json!({"jsonrpc":"2.0", "id":raw_id, "result":result}),
    )
    .await
}

async fn reject_mismatched_host_response(
    writer: &SharedWriter,
    pending: PendingHostRequest,
) -> Result<(), SessionError> {
    match pending {
        PendingHostRequest::FolderTrust { raw_id, .. } => {
            write_json_line(
                writer,
                &folder_trust_response_frame(&raw_id, FolderTrustUserDecision::KeepGated),
            )
            .await
        }
        pending => {
            let raw_id = pending.raw_id().clone();
            reply_rpc_error(
                writer,
                raw_id,
                -32_602,
                "host response did not match pending request",
            )
            .await;
            Ok(())
        }
    }
}

async fn reply_rpc_error(writer: &SharedWriter, raw_id: Value, code: i64, message: &str) {
    let _ = write_json_line(
        writer,
        &json!({
            "jsonrpc":"2.0", "id":raw_id,
            "error":{"code":code, "message":message}
        }),
    )
    .await;
}

async fn write_json_line(writer: &SharedWriter, frame: &Value) -> Result<(), SessionError> {
    let mut bytes = serde_json::to_vec(frame)
        .map_err(|e| SessionError::Send(format!("encode ACP frame: {e}")))?;
    if bytes.len() > MAX_OUTBOUND_BYTES {
        return Err(SessionError::Send(
            "ACP outbound frame exceeded the 64 MiB safety limit".to_string(),
        ));
    }
    bytes.push(b'\n');
    let mut guard = writer.lock().await;
    let stdin = guard
        .as_mut()
        .ok_or_else(|| SessionError::Send("ACP stdin is closed".to_string()))?;
    stdin
        .write_all(&bytes)
        .await
        .map_err(|e| SessionError::Send(format!("write ACP frame: {e}")))?;
    stdin
        .flush()
        .await
        .map_err(|e| SessionError::Send(format!("flush ACP frame: {e}")))
}

enum FrameRead {
    Line(String),
    Oversized,
}

async fn read_bounded_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<FrameRead>> {
    let mut bytes = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() && !oversized {
                return Ok(None);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(available.len(), |index| index + 1);
        if !oversized {
            let remaining = MAX_FRAME_BYTES.saturating_sub(bytes.len());
            if take > remaining {
                oversized = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(take);
        if newline.is_some() {
            break;
        }
    }
    if oversized {
        return Ok(Some(FrameRead::Oversized));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }
    let line = String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, error.utf8_error())
    })?;
    Ok(Some(FrameRead::Line(line)))
}

fn validate_initialize(vendor: AcpVendor, result: &Value) -> Result<(), SessionError> {
    if result.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Err(SessionError::Start(
            "ACP agent did not negotiate protocol version 1".to_string(),
        ));
    }
    // For Kimi, require the peer to be RECOGNIZABLY Kimi Code (official identity +
    // a parseable version) — a genuinely different program on PATH (a name
    // collision, e.g. the retired Python `kimi`) is still rejected with a clear
    // error. But a newer Kimi Code release is NOT rejected on version: it degrades
    // exactly like Grok, running on the baseline ACP contract with the enhanced
    // source-verified capabilities simply off (`kimi_source_contract == false` in
    // `negotiated_capabilities`). So a routine Kimi upgrade keeps working.
    if matches!(vendor, AcpVendor::Kimi)
        && !kimi_source_profile_from_initialize(result).is_kimi_code_identity()
    {
        return Err(SessionError::Start(
            "the `kimi` on PATH did not identify itself as Kimi Code CLI over ACP — check that Kimi Code (not another `kimi`) is installed, or set UMADEV_KIMI_BIN to the Kimi Code executable".to_string(),
        ));
    }
    Ok(())
}

fn negotiated_capabilities(vendor: AcpVendor, result: &Value) -> NegotiatedCapabilities {
    let source_profile = source_profile_from_initialize(result);
    let kimi_source_profile = kimi_source_profile_from_initialize(result);
    let grok_source_contract =
        matches!(vendor, AcpVendor::Grok) && source_profile.is_audited_version();
    let kimi_source_contract =
        matches!(vendor, AcpVendor::Kimi) && kimi_source_profile.is_audited_version();
    NegotiatedCapabilities {
        // The published Grok agent accepts ContentBlock::Image in its prompt
        // parser but currently omits the standard image flag from initialize.
        // Recover that vendor-specific capability only behind Grok's own
        // versioned identity marker; unknown ACP peers remain advertisement-only.
        image: advertised_bool(
            result
                .pointer("/agentCapabilities/promptCapabilities/image")
                .or_else(|| result.pointer("/agent_capabilities/prompt_capabilities/image")),
        ) || (matches!(vendor, AcpVendor::Grok)
            && source_profile.supports(GrokSourceCapability::ImagePromptFallback)),
        embedded_context: advertised_bool(
            result
                .pointer("/agentCapabilities/promptCapabilities/embeddedContext")
                .or_else(|| {
                    result.pointer("/agent_capabilities/prompt_capabilities/embedded_context")
                }),
        ),
        resume: advertised_presence(
            result
                .pointer("/agentCapabilities/sessionCapabilities/resume")
                .or_else(|| result.pointer("/agent_capabilities/session_capabilities/resume")),
        ),
        load: load_session_supported(result),
        close: advertised_presence(
            result
                .pointer("/agentCapabilities/sessionCapabilities/close")
                .or_else(|| result.pointer("/agent_capabilities/session_capabilities/close")),
        ),
        interject: matches!(vendor, AcpVendor::Grok)
            && source_profile.supports(GrokSourceCapability::Interject),
        prompt_queue: matches!(vendor, AcpVendor::Grok)
            && source_profile.supports(GrokSourceCapability::PromptQueue),
        folder_trust: matches!(vendor, AcpVendor::Grok)
            && source_profile.supports(GrokSourceCapability::FolderTrust),
        background_process_control: matches!(vendor, AcpVendor::Grok)
            && source_profile.supports(GrokSourceCapability::BackgroundProcessControl),
        // The pinned Grok source implements both standard methods, but a
        // session's model switch surface is enabled only after session/new or
        // session/load also returns the source-shaped model catalog.
        set_model: false,
        set_mode: grok_source_contract || kimi_source_contract,
        grok_source_contract,
        kimi_source_contract,
    }
}

fn recovery_method(capabilities: NegotiatedCapabilities) -> Option<&'static str> {
    if capabilities.resume {
        Some("session/resume")
    } else if capabilities.load {
        Some("session/load")
    } else {
        None
    }
}

fn advertised_bool(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

fn advertised_presence(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null() && value.as_bool() != Some(false))
}

fn load_session_supported(result: &Value) -> bool {
    result
        .pointer("/agentCapabilities/loadSession")
        .or_else(|| result.pointer("/agent_capabilities/load_session"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn advertised_auth_methods(result: &Value) -> Option<&[Value]> {
    result
        .get("authMethods")
        .or_else(|| result.get("auth_methods"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
}

fn safe_grok_auth_method(result: &Value, source_default_is_audited: bool) -> Option<&str> {
    let methods = advertised_auth_methods(result)?;
    let ids = || {
        methods.iter().filter_map(|method| {
            method
                .get("id")
                .or_else(|| method.get("methodId"))
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= MAX_DIAGNOSTIC_CHARS)
        })
    };
    if source_default_is_audited {
        if let Some(default) = result
            .pointer("/_meta/defaultAuthMethodId")
            .or_else(|| result.pointer("/_meta/default_auth_method_id"))
            .and_then(Value::as_str)
            .filter(|id| auth_method_is_noninteractive(id))
        {
            if let Some(advertised) = ids().find(|id| *id == default) {
                return Some(advertised);
            }
        }
    }
    ids()
        .find(|id| id.eq_ignore_ascii_case("cached_token"))
        .or_else(|| ids().find(|id| id.eq_ignore_ascii_case("xai.api_key")))
        .or_else(|| ids().find(|id| auth_method_is_noninteractive(id)))
}

fn auth_method_is_noninteractive(method_id: &str) -> bool {
    matches!(
        method_id.to_ascii_lowercase().as_str(),
        "xai.api_key" | "cached_token" | "cached" | "existing_session" | "local_session"
    )
}

fn validate_grok_auth_gate(result: &Value) -> Result<(), SessionError> {
    let Some(gate) = result.pointer("/_meta/gate").filter(|gate| !gate.is_null()) else {
        return Ok(());
    };
    let message = gate
        .get("message")
        .and_then(Value::as_str)
        .map(redact_text)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Grok Build account access is gated".to_string());
    let label = gate
        .get("label")
        .and_then(Value::as_str)
        .map(redact_text)
        .filter(|value| !value.trim().is_empty());
    let url = gate.get("url").and_then(Value::as_str).filter(|value| {
        value.len() <= 2_048
            && url::Url::parse(value)
                .ok()
                .is_some_and(|url| matches!(url.scheme(), "https" | "http"))
    });
    let mut detail = clip_text(&message, MAX_DIAGNOSTIC_CHARS);
    if let Some(label) = label {
        detail.push_str(": ");
        detail.push_str(&clip_text(&label, 160));
    }
    if let Some(url) = url {
        detail.push_str(" — ");
        detail.push_str(url);
    }
    Err(SessionError::Start(detail))
}

fn select_session_mode(
    setup: &Value,
    vendor: AcpVendor,
    permissions: BasePermissionProfile,
    source_contract: bool,
) -> Option<String> {
    if source_contract {
        return Some(
            match (vendor, permissions) {
                (AcpVendor::Grok | AcpVendor::Kimi, BasePermissionProfile::Plan) => "plan",
                (
                    AcpVendor::Grok | AcpVendor::Kimi,
                    BasePermissionProfile::Guarded | BasePermissionProfile::Auto,
                ) => "default",
            }
            .to_string(),
        );
    }
    let available = setup
        .get("modes")
        .and_then(|modes| {
            modes
                .get("availableModes")
                .or_else(|| modes.get("available_modes"))
        })
        .and_then(Value::as_array)
        .or_else(|| config_option(setup, "mode")?.get("options")?.as_array())?;
    let wanted: &[&str] = match (vendor, permissions) {
        (AcpVendor::Grok, BasePermissionProfile::Plan) => &["plan", "read-only", "readonly", "ask"],
        (AcpVendor::Grok, BasePermissionProfile::Guarded) => &["default", "ask", "agent", "code"],
        (AcpVendor::Grok, BasePermissionProfile::Auto) => {
            &["bypassPermissions", "bypass_permissions", "yolo", "auto"]
        }
        (AcpVendor::Kimi, BasePermissionProfile::Plan) => &["plan"],
        (AcpVendor::Kimi, BasePermissionProfile::Guarded | BasePermissionProfile::Auto) => {
            &["default"]
        }
    };
    wanted.iter().find_map(|wanted| {
        available.iter().find_map(|mode| {
            let id = mode
                .get("id")
                .or_else(|| mode.get("value"))
                .and_then(Value::as_str)?;
            id.eq_ignore_ascii_case(wanted).then(|| id.to_string())
        })
    })
}

async fn resolve_vendor_program(vendor: AcpVendor) -> Result<String, SessionError> {
    if let Ok(program) = std::env::var(vendor.program_env()) {
        if !program.trim().is_empty() {
            let program = resolve_and_validate_vendor_program(vendor, program.trim())?;
            return require_resolved_vendor_program(vendor, program).await;
        }
    }
    if matches!(vendor, AcpVendor::Grok) {
        if let Some(program) = grok_canonical_native_program() {
            if version_output(&program).await.is_some() {
                return Ok(program);
            }
        }
    }
    let program = resolve_and_validate_vendor_program(vendor, vendor.primary_program())?;
    require_resolved_vendor_program(vendor, program).await
}

async fn require_resolved_vendor_program(
    vendor: AcpVendor,
    program: String,
) -> Result<String, SessionError> {
    // Only require that the program is installed and answers `--version`. A version
    // UmaDev has not source-audited is NOT refused here: the session runs on the
    // baseline ACP protocol (protocolVersion 1 is still enforced at
    // `validate_initialize`), and only the enhanced, source-verified capabilities
    // stay gated behind the audited version (see `negotiated_capabilities`). This
    // matches Grok, which already degrades instead of blocking — so a routine
    // upstream Kimi upgrade no longer makes the base unusable.
    if version_output(&program).await.is_none() {
        return Err(SessionError::Start(format!(
            "{} is not installed or not on PATH",
            vendor.display_name()
        )));
    }
    Ok(program)
}

fn resolve_and_validate_vendor_program(
    vendor: AcpVendor,
    program: &str,
) -> Result<String, SessionError> {
    #[cfg(windows)]
    let resolved = if matches!(vendor, AcpVendor::Grok) {
        resolve_grok_windows_native_program(program)
    } else {
        resolve_program(program)
    };
    #[cfg(not(windows))]
    let resolved = resolve_program(program);
    validate_vendor_program(vendor, &resolved)
}

#[cfg(windows)]
fn resolve_grok_windows_native_program(program: &str) -> String {
    // ACP arguments must never pass through cmd.exe. A bare `grok` therefore
    // searches only for the native PE, independent of PATHEXT ordering; a PATH
    // containing only npm's grok.cmd intentionally remains unresolved.
    if program.contains('/') || program.contains('\\') {
        return program.to_string();
    }
    let extension = Path::new(program)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if !extension.is_empty() && !extension.eq_ignore_ascii_case("exe") {
        return program.to_string();
    }
    let native_name = if extension.eq_ignore_ascii_case("exe") {
        program.to_string()
    } else {
        format!("{program}.exe")
    };
    if let Some(canonical) = grok_canonical_native_program() {
        if Path::new(&canonical)
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.eq_ignore_ascii_case(&native_name))
        {
            return canonical;
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(&native_name);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    native_name
}

fn grok_canonical_native_program() -> Option<String> {
    let name = if cfg!(windows) { "grok.exe" } else { "grok" };
    let path = home_dir()?.join(".grok").join("bin").join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

fn validate_vendor_program(vendor: AcpVendor, program: &str) -> Result<String, SessionError> {
    validate_vendor_program_for_target(vendor, program, cfg!(windows))
}

fn validate_vendor_program_for_target(
    vendor: AcpVendor,
    program: &str,
    windows: bool,
) -> Result<String, SessionError> {
    if matches!(vendor, AcpVendor::Grok) && windows {
        let extension = Path::new(program)
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        if !extension.eq_ignore_ascii_case("exe") {
            return Err(SessionError::Start(
                "Grok Build ACP requires the official native grok.exe on Windows; shell shims and extensionless launchers cannot safely carry protocol arguments. Install the native CLI under ~/.grok/bin or point UMADEV_GROK_BIN to grok.exe"
                    .to_string(),
            ));
        }
    }
    Ok(program.to_string())
}

async fn version_output(program: &str) -> Option<String> {
    let workspace = default_workspace();
    let output = run_subprocess(SubprocessCall {
        program,
        args: &["--version".to_string()],
        prompt: "",
        channel: PromptChannel::Stdin,
        workspace: &workspace,
        timeout: Duration::from_secs(10),
        env: &[],
    })
    .await
    .ok()?;
    output
        .stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| clip_text(line.trim(), 200))
}

fn suppress_notification(method: &str, frame: &Value) -> bool {
    method.starts_with("x.ai/")
        || method.starts_with("_x.ai/")
        || method.ends_with("mcp/servers_updated")
        || contains_sensitive_key(frame)
}

fn contains_sensitive_key(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| is_sensitive_key(key) || contains_sensitive_key(value)),
        Value::Array(values) => values.iter().any(contains_sensitive_key),
        _ => false,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if matches!(
        normalized.as_str(),
        "env" | "environment" | "environmentvariables" | "headers" | "httpheaders"
    ) {
        return true;
    }
    [
        "token",
        "accesstoken",
        "refreshtoken",
        "authorization",
        "apikey",
        "password",
        "secret",
        "credential",
        "cookie",
        "privatekey",
    ]
    .iter()
    .any(|needle| normalized == *needle || normalized.ends_with(needle))
}

fn sanitize_value(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    if is_sensitive_key(&key) {
                        (key, Value::String("[redacted]".to_string()))
                    } else {
                        (key, sanitize_value(value))
                    }
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(sanitize_value).collect()),
        Value::String(text) => Value::String(redact_text(&text)),
        other => other,
    }
}

fn safe_rpc_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| "ACP request failed".to_string(), redact_text);
    match code {
        Some(code) => format!(
            "ACP error {code}: {}",
            clip_text(&message, MAX_DIAGNOSTIC_CHARS)
        ),
        None => clip_text(&message, MAX_DIAGNOSTIC_CHARS),
    }
}

fn redact_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if [
        "authorization:",
        "bearer ",
        "api_key",
        "api-key",
        "access_token",
        "refresh_token",
        "password=",
        "private_key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "[redacted sensitive content]".to_string()
    } else {
        text.to_string()
    }
}

fn clip_text(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_string(),
    }
}

fn rpc_id_string(raw_id: &Value) -> String {
    match raw_id {
        Value::String(id) => format!("s:{id}"),
        Value::Number(id) => format!("n:{id}"),
        _ => "unknown".to_string(),
    }
}

fn is_standard_ask_question_method(method: &str) -> bool {
    matches!(method, "session/ask_question" | "session/request_input")
}

fn is_grok_ask_user_question_method(method: &str) -> bool {
    method == "x.ai/ask_user_question"
}

fn is_permission_expansion_method(method: &str) -> bool {
    matches!(
        method,
        "session/request_permission_expansion" | "session/request_sandbox"
    )
}

fn is_standard_plan_confirmation_method(method: &str) -> bool {
    matches!(method, "session/confirm_plan" | "session/plan_confirmation")
}

fn is_grok_exit_plan_mode_method(method: &str) -> bool {
    method == "x.ai/exit_plan_mode"
}

fn handshake_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("UMADEV_ACP_HANDSHAKE_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|seconds| *seconds > 0)
            .unwrap_or(DEFAULT_HANDSHAKE_SECS),
    )
}

#[cfg(test)]
mod prompt_queue_tests;

#[cfg(test)]
mod folder_trust_tests;

#[cfg(test)]
mod background_control_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Cursor, Write as _};

    fn fixture_path(name: &str) -> PathBuf {
        std::env::temp_dir().join("umadev-acp-fixtures").join(name)
    }

    fn grok_task_backgrounded(
        session_id: &str,
        event_id: &str,
        task_id: &str,
        tool_call_id: &str,
    ) -> Value {
        json!({
            "sessionId":session_id,
            "_meta":{"eventId":event_id},
            "update":{
                "sessionUpdate":"task_backgrounded",
                "tool_call_id":tool_call_id,
                "task_id":task_id,
                "command":"pnpm dev",
                "cwd":"/workspace/project",
                "output_file":"/workspace/project/.grok/task.log",
                "monitor_description":null,
                "description":"Development server"
            }
        })
    }

    fn grok_task_completed(session_id: &str, event_id: &str, task_id: &str, kind: &str) -> Value {
        json!({
            "sessionId":session_id,
            "_meta":{"eventId":event_id},
            "update":{
                "sessionUpdate":"task_completed",
                "task_snapshot":{
                    "task_id":task_id,
                    "command":"pnpm dev",
                    "display_command":null,
                    "cwd":"/workspace/project",
                    "start_time":{"secs_since_epoch":1,"nanos_since_epoch":0},
                    "end_time":{"secs_since_epoch":2,"nanos_since_epoch":0},
                    "output":"ready",
                    "output_file":"/workspace/project/.grok/task.log",
                    "truncated":false,
                    "exit_code":0,
                    "signal":null,
                    "completed":true,
                    "kind":kind,
                    "block_waited":false,
                    "explicitly_killed":false,
                    "owner_session_id":session_id
                },
                "will_wake":false
            }
        })
    }

    #[test]
    fn fake_grok_interactive_auth_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_grok_interactive_auth_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    use std::fs::OpenOptions;
                    let mut log = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("auth-init.log")
                        .unwrap();
                    writeln!(log, "{}", std::process::id()).unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{
                                "protocolVersion":1,
                                "agentCapabilities":{},
                                "authMethods":[
                                    {"id":"grok.com","name":"Grok"},
                                    {"id":"oidc","name":"Enterprise SSO"}
                                ],
                                "_meta":{
                                    "grokShell":true,
                                    "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION,
                                    "defaultAuthMethodId":null
                                }
                            }
                        })
                    )
                    .unwrap();
                }
                Some("authenticate") => {
                    assert_eq!(frame["params"]["methodId"], "grok.com");
                    assert_eq!(frame["params"]["_meta"]["use_oauth"], false);
                    assert!(frame["params"]["_meta"].get("headless").is_none());
                    std::fs::write("browser-capable-auth.seen", b"yes").unwrap();
                    // Keep authenticate pending while get_url is serviced.
                }
                Some("x.ai/auth/get_url") => {
                    assert_eq!(frame["params"], json!({}));
                    assert!(std::path::Path::new("browser-capable-auth.seen").exists());
                    std::fs::write("get-url.seen", b"yes").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{
                                "auth_url":"http://127.0.0.1:43123/callback?state=fixture",
                                "external_provider":false,
                                "mode":"loopback"
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/new") => {
                    std::fs::write("session-new.unexpected", b"bad").unwrap();
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn grok_interactive_auth_requires_offer_then_fresh_confirmed_child_and_reaps() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_interactive_auth_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];

        let first = AcpSession::start_with_program_args_and_policy(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args.clone(),
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            SessionOpenPolicy::NonInteractive,
        )
        .await;
        let offer = match first {
            Err(SessionOpenError::AuthRequired(offer)) => offer,
            Err(error) => panic!("expected typed auth offer, got {error}"),
            Ok(_) => panic!("interactive auth must not start without confirmation"),
        };
        assert!(offer.methods.iter().any(|method| method.id == "grok.com"));
        assert!(!workspace.path().join("browser-capable-auth.seen").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("auth-init.log"))
                .unwrap()
                .lines()
                .count(),
            1
        );

        let (events, mut event_rx) = mpsc::unbounded_channel();
        let open = AcpSession::start_with_program_args_and_policy(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            SessionOpenPolicy::UserAuthorized {
                attempt_id: SessionOpenId::new(77),
                method_id: "grok.com".to_string(),
                events,
            },
        );
        tokio::pin!(open);
        let event = tokio::select! {
            result = &mut open => match result {
                Ok(_) => panic!("open completed before challenge"),
                Err(error) => panic!("open failed before challenge: {error}"),
            },
            event = event_rx.recv() => event.expect("challenge event"),
        };
        let SessionOpenEvent::Challenge { challenge, control } = event else {
            panic!("expected challenge");
        };
        assert_eq!(challenge.attempt_id, SessionOpenId::new(77));
        assert!(challenge.accepts_manual_code());
        control.try_cancel().unwrap();
        let result = open.await;
        assert!(matches!(
            result,
            Err(SessionOpenError::Session(SessionError::Start(ref message)))
                if message.contains("cancelled")
        ));

        assert!(workspace.path().join("browser-capable-auth.seen").exists());
        assert!(workspace.path().join("get-url.seen").exists());
        assert!(!workspace.path().join("session-new.unexpected").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("auth-init.log"))
                .unwrap()
                .lines()
                .count(),
            2,
            "confirmation must use a fresh child and fresh initialize"
        );
    }

    #[test]
    fn fake_grok_cached_then_auth_required_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args()
                .any(|arg| arg.ends_with("fake_grok_cached_then_auth_required_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            let response = match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{
                        "protocolVersion":1,
                        "agentCapabilities":{},
                        "authMethods":[
                            {"id":"cached_token","name":"Cached token"},
                            {"id":"grok.com","name":"Grok"}
                        ],
                        "_meta":{
                            "grokShell":true,
                            "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION,
                            "defaultAuthMethodId":"cached_token"
                        }
                    }
                })),
                Some("authenticate") => {
                    std::fs::write("unexpected-authenticate.seen", b"bad").unwrap();
                    None
                }
                Some("session/new") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "error":{"code":-32000,"message":"authentication required"}
                })),
                _ => None,
            };
            if let Some(response) = response {
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn cached_session_new_auth_required_returns_typed_offer_without_browser_fallback() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_cached_then_auth_required_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let result = AcpSession::start_with_program_args_and_policy(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            SessionOpenPolicy::NonInteractive,
        )
        .await;
        let offer = match result {
            Err(SessionOpenError::AuthRequired(offer)) => offer,
            Err(error) => panic!("expected typed auth offer, got {error}"),
            Ok(_) => panic!("session/new auth_required must not create a session"),
        };
        assert_eq!(offer.default_method_id.as_deref(), Some("cached_token"));
        assert!(offer.methods.iter().any(|method| method.id == "grok.com"));
        assert!(!workspace
            .path()
            .join("unexpected-authenticate.seen")
            .exists());
    }

    #[test]
    fn vendor_launch_args_lock_permissions_and_grok_ordering() {
        let workspace = Path::new("/tmp/project");
        assert_eq!(
            AcpVendor::Grok.install_hint(),
            AcpVendor::Grok.install_hint_for(cfg!(windows))
        );
        assert_eq!(
            AcpVendor::Grok.install_hint_for(false),
            "curl -fsSL https://x.ai/cli/install.sh | bash"
        );
        assert_eq!(
            AcpVendor::Grok.install_hint_for(true),
            "irm https://x.ai/cli/install.ps1 | iex"
        );
        let plan = AcpVendor::Grok.args(workspace, "", BasePermissionProfile::Plan);
        assert_eq!(
            plan,
            [
                "--no-auto-update",
                "--cwd",
                "/tmp/project",
                "--permission-mode",
                "plan",
                "--sandbox",
                "read-only",
                "agent",
                "--no-leader",
                "stdio"
            ]
        );

        let guarded = AcpVendor::Grok.args(workspace, "", BasePermissionProfile::Guarded);
        assert_eq!(
            guarded,
            [
                "--no-auto-update",
                "--cwd",
                "/tmp/project",
                "--permission-mode",
                "default",
                "--sandbox",
                "off",
                "agent",
                "--no-leader",
                "stdio"
            ]
        );

        let auto = AcpVendor::Grok.args(workspace, "grok-4.5", BasePermissionProfile::Auto);
        assert_eq!(
            auto,
            [
                "--no-auto-update",
                "--cwd",
                "/tmp/project",
                "--permission-mode",
                "bypassPermissions",
                "--sandbox",
                "off",
                "agent",
                "--no-leader",
                "--model",
                "grok-4.5",
                "--always-approve",
                "stdio"
            ]
        );
        let agent_index = auto.iter().position(|arg| arg == "agent").unwrap();
        let no_leader_index = auto.iter().position(|arg| arg == "--no-leader").unwrap();
        let model_index = auto.iter().position(|arg| arg == "--model").unwrap();
        let stdio_index = auto.iter().position(|arg| arg == "stdio").unwrap();
        assert!(
            agent_index < no_leader_index
                && no_leader_index < model_index
                && model_index < stdio_index
        );
        assert_eq!(
            plan.iter()
                .filter(|arg| arg.as_str() == "--no-leader")
                .count(),
            1
        );
        assert!(!plan
            .iter()
            .any(|arg| matches!(arg.as_str(), "--no-subagents" | "--tools")));

        assert_eq!(
            AcpVendor::Kimi.install_hint(),
            "npm install -g @moonshot-ai/kimi-code"
        );
        for profile in [
            BasePermissionProfile::Plan,
            BasePermissionProfile::Guarded,
            BasePermissionProfile::Auto,
        ] {
            assert_eq!(AcpVendor::Kimi.args(workspace, "", profile), ["acp"]);
        }
    }

    #[test]
    fn legacy_acp_drivers_ignore_external_session_id_pins() {
        for vendor in [AcpVendor::Grok, AcpVendor::Kimi] {
            let mut driver = AcpDriver::new(vendor);
            let fresh_driver = format!("{driver:?}");

            driver.set_session_id(Some("caller-generated-session".to_string()));

            assert_eq!(format!("{driver:?}"), fresh_driver);
        }
    }

    #[test]
    fn a_newer_kimi_is_accepted_and_only_a_wrong_identity_is_rejected() {
        // The reported regression: a routine Kimi upgrade (0.26.0 → 0.27.0) must NOT
        // make the base unusable. A newer, correctly-identified Kimi Code passes
        // validation and runs on the baseline contract (audited enhancements off);
        // only a genuinely different program on PATH (wrong ACP identity) is refused.
        let newer = json!({
            "protocolVersion":1,
            "agentInfo":{"name":"Kimi Code CLI","version":"0.27.0"},
        });
        assert!(
            validate_initialize(AcpVendor::Kimi, &newer).is_ok(),
            "a newer Kimi Code release must be accepted, not refused"
        );
        assert!(
            !negotiated_capabilities(AcpVendor::Kimi, &newer).kimi_source_contract,
            "the audited source contract stays gated behind the exact audited version"
        );

        let wrong_program = json!({
            "protocolVersion":1,
            "agentInfo":{"name":"Kimi CLI","version":"0.53.0"},
        });
        assert!(
            validate_initialize(AcpVendor::Kimi, &wrong_program).is_err(),
            "a different `kimi` on PATH (wrong ACP identity) is still rejected"
        );
    }

    #[test]
    fn kimi_windows_launch_accepts_npm_shims_while_grok_does_not() {
        for shim in [r"C:\npm\kimi.cmd", r"C:\npm\KIMI.BAT"] {
            assert_eq!(
                validate_vendor_program_for_target(AcpVendor::Kimi, shim, true).unwrap(),
                shim
            );
        }
    }

    #[test]
    fn grok_windows_launch_rejects_shell_shims_after_resolution() {
        for shim in [r"C:\npm\grok.cmd", r"C:\npm\GROK.BAT"] {
            let error = validate_vendor_program_for_target(AcpVendor::Grok, shim, true)
                .expect_err("Windows shell shims must never reach ACP launch");
            assert!(error.to_string().contains("grok.exe"));
        }
        assert_eq!(
            validate_vendor_program_for_target(
                AcpVendor::Grok,
                r"C:\Users\me\.grok\bin\grok.exe",
                true
            )
            .unwrap(),
            r"C:\Users\me\.grok\bin\grok.exe"
        );
        assert_eq!(
            validate_vendor_program_for_target(AcpVendor::Grok, "/opt/grok/bin/grok", false)
                .unwrap(),
            "/opt/grok/bin/grok"
        );
    }

    #[test]
    fn grok_private_wire_normalization_is_exact_and_wrapper_safe() {
        let direct = json!({
            "method":"_x.ai/ask_user_question",
            "params":{"sessionId":"s-1","questions":[]}
        });
        let normalized = normalize_server_message(&direct).unwrap();
        assert_eq!(normalized.method, "x.ai/ask_user_question");
        assert_eq!(normalized.params["sessionId"], "s-1");

        let wrapped = json!({
            "method":"_x.ai/exit_plan_mode",
            "params":{
                "method":"x.ai/exit_plan_mode",
                "params":{"sessionId":"s-2","plan":"ship it"}
            }
        });
        let normalized = normalize_server_message(&wrapped).unwrap();
        assert_eq!(normalized.method, "x.ai/exit_plan_mode");
        assert_eq!(normalized.params["sessionId"], "s-2");

        let standard = json!({
            "method":"session/update",
            "params":{"method":"x.ai/not-a-wrapper","params":{"sessionId":"nested"}}
        });
        let normalized = normalize_server_message(&standard).unwrap();
        assert_eq!(normalized.method, "session/update");
        assert_eq!(normalized.params["method"], "x.ai/not-a-wrapper");

        for malformed in [
            json!({
                "method":"_x.ai/ask_user_question",
                "params":{"method":"x.ai/exit_plan_mode","params":{}}
            }),
            json!({
                "method":"_x.ai/ask_user_question",
                "params":{"method":7,"params":{}}
            }),
            json!({
                "method":"_x.ai/ask_user_question",
                "params":{"method":"x.ai/ask_user_question"}
            }),
        ] {
            assert!(normalize_server_message(&malformed).is_err());
        }
    }

    #[test]
    fn grok_firmware_stays_off_argv_and_uses_fresh_session_metadata() {
        let workspace = Path::new("/tmp/project");
        let rules = "firmware with ünicode and 中文";
        let launch = AcpVendor::Grok.launch(
            workspace,
            "grok-model",
            BasePermissionProfile::Guarded,
            Some(rules),
        );
        assert!(launch.args.starts_with(&[
            "--no-auto-update".to_string(),
            "--cwd".to_string(),
            "/tmp/project".to_string(),
        ]));
        assert!(!launch.args.iter().any(|arg| arg == "--rules"));
        assert!(!launch.args.iter().any(|arg| arg.contains("firmware with")));
        assert_eq!(launch.append_system.as_deref(), Some(rules));
        let meta = grok_session_meta(launch.append_system.as_deref(), Some("grok-model"));
        assert_eq!(meta["rules"], rules);
        assert_eq!(meta["modelId"], "grok-model");
        assert_eq!(meta["clientIdentifier"], "umadev");

        for blank in [None, Some(""), Some("   ")] {
            let launch =
                AcpVendor::Grok.launch(workspace, "", BasePermissionProfile::Guarded, blank);
            assert!(!launch.args.iter().any(|arg| arg == "--rules"));
            assert_eq!(launch.append_system, None);
            assert!(grok_session_meta(launch.append_system.as_deref(), None)
                .get("rules")
                .is_none());
        }
    }

    #[test]
    fn grok_prompt_metadata_locks_turn_mode_identity_and_correlation() {
        let plan = grok_prompt_meta(BasePermissionProfile::Plan, 7);
        assert_eq!(plan["mode"], "plan");
        assert_eq!(plan["clientIdentifier"], "umadev");
        assert!(plan["promptId"]
            .as_str()
            .is_some_and(|id| id.starts_with("umadev-") && id.ends_with("-7")));
        assert_eq!(
            grok_prompt_meta(BasePermissionProfile::Guarded, 8)["mode"],
            "agent"
        );
        assert_eq!(
            grok_prompt_meta(BasePermissionProfile::Auto, 9)["mode"],
            "agent"
        );
    }

    #[test]
    fn grok_source_notifications_emit_deduplicated_lifecycle_only() {
        let spawn = json!({
            "sessionId":"active",
            "update":{
                "sessionUpdate":"subagent_spawned",
                "subagentId":"agent-1",
                "parentSessionId":"active",
                "childSessionId":"child-1",
                "subagentType":"general-purpose",
                "description":"audit"
            },
            "_meta":{"eventId":"active-1"}
        });
        let mut state = ToolState::default();
        assert_eq!(
            grok_subagent_lifecycle_event(&spawn, &mut state, false),
            None,
            "an arbitrary ACP peer cannot claim Grok private authority"
        );
        assert_eq!(
            grok_subagent_lifecycle_event(&spawn, &mut state, true),
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started {
                    id: "agent-1".to_string()
                }
            ))
        );
        assert_eq!(
            grok_subagent_lifecycle_event(&spawn, &mut state, true),
            None,
            "replayed eventId must be idempotent"
        );
        let progress = json!({
            "sessionId":"active",
            "update":{"sessionUpdate":"subagent_progress","subagent_id":"agent-1"},
            "_meta":{"eventId":"active-2"}
        });
        assert_eq!(
            grok_subagent_lifecycle_event(&progress, &mut state, true),
            None
        );
        let finished = json!({
            "sessionId":"active",
            "update":{
                "sessionUpdate":"subagent_finished",
                "subagent_id":"agent-1",
                "child_session_id":"child-1",
                "status":"completed"
            },
            "_meta":{"eventId":"active-3"}
        });
        assert_eq!(
            grok_subagent_lifecycle_event(&finished, &mut state, true),
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Finished {
                    id: "agent-1".to_string()
                }
            ))
        );
        for index in 0..(MAX_SEEN_GROK_EVENT_IDS + 3) {
            assert!(state.remember_grok_event(&format!("bounded-{index}")));
        }
        assert_eq!(state.seen_grok_event_ids.len(), MAX_SEEN_GROK_EVENT_IDS);
        assert!(!state.seen_grok_event_ids.contains("bounded-0"));

        let active: ActiveSessionId = Arc::new(RwLock::new(Some("active".to_string())));
        let foreign = json!({
            "sessionId":"stale","update":{"sessionUpdate":"subagent_spawned"}
        });
        assert!(!acp_message_matches_active_session(&foreign, &active));
    }

    fn assert_private_background_start(
        tools: &mut ToolState,
        tracker: &mut BackgroundProcessTracker,
    ) {
        let mut started = grok_task_backgrounded("active", "bg-1", "task-1", "call-1");
        started["update"]["command"] = json!("echo API_KEY=private-command");
        started["update"]["cwd"] = json!("/Users/private/project");
        started["update"]["output_file"] = json!("/Users/private/task.log");
        started["update"]["description"] = json!("Authorization: Bearer private-description");

        assert!(grok_background_process_lifecycle_event(
            "x.ai/task_backgrounded",
            &started,
            &mut *tools,
            false,
        )
        .is_none());
        let parsed = grok_background_process_lifecycle_event(
            "x.ai/task_backgrounded",
            &started,
            &mut *tools,
            true,
        )
        .unwrap();
        let event = tracker.apply(parsed, false).unwrap();
        let rendered = format!("{event:?}");
        assert!(!rendered.contains("private-command"));
        assert!(!rendered.contains("/Users/private"));
        assert!(!rendered.contains("private-description"));
        assert!(matches!(
            event,
            SessionEvent::BackgroundProcess(BackgroundProcessSignal::Started {
                process: BackgroundProcessInfo {
                    kind: BackgroundProcessKind::Bash,
                    ..
                }
            })
        ));
        assert!(grok_background_process_lifecycle_event(
            "x.ai/task_backgrounded",
            &started,
            &mut *tools,
            true,
        )
        .is_none());
    }

    fn assert_out_of_order_background_completion(
        tools: &mut ToolState,
        tracker: &mut BackgroundProcessTracker,
    ) {
        let completed = grok_task_completed("active", "done-late", "task-late", "monitor");
        let parsed = grok_background_process_lifecycle_event(
            "x.ai/task_completed",
            &completed,
            &mut *tools,
            true,
        )
        .unwrap();
        assert!(matches!(
            tracker.apply(parsed, false),
            Some(SessionEvent::BackgroundProcess(
                BackgroundProcessSignal::Finished {
                    ref task_id,
                    kind: BackgroundProcessKind::Monitor,
                    ..
                }
            )) if task_id == "task-late"
        ));
        let late_start = grok_task_backgrounded("active", "late-start", "task-late", "call-late");
        let parsed = grok_background_process_lifecycle_event(
            "x.ai/session/update",
            &late_start,
            &mut *tools,
            true,
        )
        .unwrap();
        assert!(tracker.apply(parsed, false).is_none());
        let duplicate_completion =
            grok_task_completed("active", "done-late-again", "task-late", "monitor");
        let parsed = grok_background_process_lifecycle_event(
            "x.ai/session/update",
            &duplicate_completion,
            &mut *tools,
            true,
        )
        .unwrap();
        assert!(tracker.apply(parsed, false).is_none());
    }

    fn assert_bounded_background_fields(tools: &mut ToolState) {
        let mut overlong = grok_task_backgrounded("active", "retryable", "task-bad", "call-bad");
        overlong["update"]["output_file"] = json!("x".repeat(MAX_BACKGROUND_PATH_CHARS + 1));
        assert!(grok_background_process_lifecycle_event(
            "x.ai/task_backgrounded",
            &overlong,
            &mut *tools,
            true,
        )
        .is_none());
        let corrected = grok_task_backgrounded("active", "retryable", "task-ok", "call-ok");
        assert!(grok_background_process_lifecycle_event(
            "x.ai/task_backgrounded",
            &corrected,
            &mut *tools,
            true,
        )
        .is_some());
        let control_id =
            grok_task_backgrounded("active", "control-id", "task\nforged", "call-control");
        assert!(grok_background_process_lifecycle_event(
            "x.ai/task_backgrounded",
            &control_id,
            &mut *tools,
            true,
        )
        .is_none());
        let mut large_output = grok_task_completed("active", "large-output", "task-large", "bash");
        large_output["update"]["task_snapshot"]["output"] = json!("x".repeat(5 * 1024 * 1024));
        assert!(grok_background_process_lifecycle_event(
            "x.ai/task_completed",
            &large_output,
            &mut *tools,
            true,
        )
        .is_some());
        let unknown = json!({
            "sessionId":"active", "_meta":{"eventId":"future-1"},
            "update":{"sessionUpdate":"task_paused"}
        });
        assert!(grok_background_process_lifecycle_event(
            "x.ai/session/update",
            &unknown,
            &mut *tools,
            true,
        )
        .is_none());
    }

    #[test]
    fn grok_background_process_parser_is_bounded_private_and_order_independent() {
        let mut tools = ToolState::default();
        let mut tracker = BackgroundProcessTracker::default();
        assert_private_background_start(&mut tools, &mut tracker);
        assert_out_of_order_background_completion(&mut tools, &mut tracker);
        assert_bounded_background_fields(&mut tools);
    }

    #[test]
    fn grok_auth_probe_treats_a_nonempty_api_key_as_logged_in() {
        // The env-key path is deterministic and independent of the machine's real
        // ~/.grok — a non-empty key is an instant LoggedIn, and the key itself is
        // never rendered into the state. (The no-key path now consults the cached
        // login file, covered by `grok_login_file_is_present_*` below, so it is not
        // asserted here where the ambient home is uncontrolled.)
        let secret = OsStr::new("xai-secret-value-must-never-be-rendered");
        let state = grok_auth_state_from_api_key(Some(secret));
        assert_eq!(state, AuthState::LoggedIn);
        assert!(!format!("{state:?}").contains(secret.to_string_lossy().as_ref()));
    }

    #[test]
    fn a_stored_credential_reads_as_login_only_on_a_nonempty_json_object() {
        // Regression: a browser/device login must read as a stored login so it no
        // longer shows "installed · login unverified" — Grok writes ~/.grok/auth.json
        // (keyed by the x.ai auth endpoint) and Kimi writes
        // ~/.kimi-code/credentials/kimi-code.json (an OAuth token set), both
        // non-empty JSON objects. An empty object, a non-object, garbage, or an
        // absent file must NOT — the probe never claims a login it cannot see.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");

        std::fs::write(&path, r#"{"https://auth.x.ai::client-abc":{"t":"…"}}"#).unwrap();
        assert!(
            credential_file_has_entries(&path),
            "a grok login is present"
        );

        std::fs::write(&path, r#"{"access_token":"…","refresh_token":"…"}"#).unwrap();
        assert!(
            credential_file_has_entries(&path),
            "a kimi login is present"
        );

        std::fs::write(&path, "{}").unwrap();
        assert!(
            !credential_file_has_entries(&path),
            "empty object is not a login"
        );

        std::fs::write(&path, "[]").unwrap();
        assert!(
            !credential_file_has_entries(&path),
            "a non-object is not a login"
        );

        std::fs::write(&path, "not json").unwrap();
        assert!(
            !credential_file_has_entries(&path),
            "garbage is not a login"
        );

        assert!(
            !credential_file_has_entries(&dir.path().join("absent.json")),
            "an absent file is not a login"
        );
    }

    #[test]
    fn fresh_acp_sessions_do_not_require_resume_or_load_capabilities() {
        let complete = json!({
            "protocolVersion":1,
            "agentCapabilities":{"loadSession":true},
            "agentInfo":{"version":"2026.07.16"}
        });
        let incomplete = json!({
            "protocolVersion":1,
            "agentCapabilities":{"loadSession":false},
            "agentInfo":{"version":"2026.07.16"}
        });
        let vendor = AcpVendor::Grok;
        assert!(
            validate_initialize(vendor, &complete).is_ok(),
            "complete long-lived contract for {vendor:?}"
        );
        assert!(
            validate_initialize(vendor, &incomplete).is_ok(),
            "fresh sessions must remain usable without optional recovery"
        );
    }

    #[test]
    fn negotiated_acp_capabilities_are_vendor_response_specific() {
        let value = json!({"agentCapabilities":{
            "loadSession":true,
            "promptCapabilities":{"image":true,"embeddedContext":false},
            "sessionCapabilities":{"resume":{},"close":{}}
        }});
        let capabilities = negotiated_capabilities(AcpVendor::Grok, &value);
        assert!(capabilities.image);
        assert!(!capabilities.embedded_context);
        assert!(capabilities.resume);
        assert!(capabilities.load);
        assert!(capabilities.close);
        assert_eq!(recovery_method(capabilities), Some("session/resume"));
        assert_eq!(
            recovery_method(NegotiatedCapabilities {
                load: true,
                ..NegotiatedCapabilities::default()
            }),
            Some("session/load")
        );
        assert_eq!(recovery_method(NegotiatedCapabilities::default()), None);
    }

    #[test]
    fn published_grok_initialize_fixture_recovers_only_source_backed_capabilities() {
        // Mirrors the open-source InitializeResponse: embedded context and load
        // are advertised; image is accepted by the published prompt parser but
        // omitted from PromptCapabilities.
        let official = json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"embeddedContext": true}
            },
            "_meta": {"grokShell": true, "agentVersion": crate::grok_contract::GROK_BUILD_SOURCE_VERSION}
        });
        let capabilities = negotiated_capabilities(AcpVendor::Grok, &official);
        assert!(capabilities.grok_source_contract);
        assert!(capabilities.image);
        assert!(capabilities.embedded_context);
        assert!(capabilities.load);
        assert!(!capabilities.resume);

        let unversioned = json!({
            "agentCapabilities": {"promptCapabilities": {"embeddedContext": true}},
            "_meta": {"grokShell": true}
        });
        let capabilities = negotiated_capabilities(AcpVendor::Grok, &unversioned);
        assert!(!capabilities.grok_source_contract);
        assert!(!capabilities.image);
    }

    #[test]
    fn published_kimi_initialize_requires_exact_identity_and_uses_only_standard_capabilities() {
        let official = json!({
            "protocolVersion":1,
            "agentInfo":{
                "name":"Kimi Code CLI",
                "version":crate::kimi_contract::KIMI_CODE_SOURCE_VERSION
            },
            "agentCapabilities":{
                "loadSession":true,
                "promptCapabilities":{"image":true,"embeddedContext":true},
                "sessionCapabilities":{"list":{},"resume":{}}
            },
            "authMethods":[{"id":"login","type":"terminal","args":["--login"]}]
        });
        validate_initialize(AcpVendor::Kimi, &official).unwrap();
        let capabilities = negotiated_capabilities(AcpVendor::Kimi, &official);
        assert!(capabilities.kimi_source_contract);
        assert!(!capabilities.grok_source_contract);
        assert!(capabilities.image && capabilities.embedded_context);
        assert!(capabilities.resume && capabilities.load && capabilities.set_mode);
        assert!(!capabilities.interject && !capabilities.prompt_queue);

        // A newer Kimi Code (right identity, unaudited version) is accepted and
        // degrades; only a WRONG identity (a different `kimi`) is rejected.
        assert!(
            validate_initialize(
                AcpVendor::Kimi,
                &json!({"protocolVersion":1,"agentInfo":{"name":"Kimi Code CLI","version":"0.26.1"}})
            )
            .is_ok(),
            "a newer Kimi Code release must not be refused on version"
        );
        assert!(
            validate_initialize(
                AcpVendor::Kimi,
                &json!({"protocolVersion":1,"agentInfo":{"name":"Kimi CLI","version":crate::kimi_contract::KIMI_CODE_SOURCE_VERSION}})
            )
            .is_err(),
            "a different `kimi` (wrong ACP identity) is still rejected"
        );

        let wire = serde_json::to_value(
            InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                acp_client_capabilities(AcpVendor::Kimi, FolderTrustClientSurface::Headless),
            ),
        )
        .unwrap();
        assert!(wire["clientCapabilities"].get("_meta").is_none());
    }

    #[test]
    fn grok_initialize_honestly_keeps_reverse_io_inside_the_base_sandbox() {
        let request = InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(acp_client_capabilities(
                AcpVendor::Grok,
                FolderTrustClientSurface::Headless,
            ))
            .meta(grok_initialize_meta().as_object().cloned());
        let wire = serde_json::to_value(request).unwrap();
        assert_eq!(
            wire.pointer("/clientCapabilities/fs/readTextFile"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            wire.pointer("/clientCapabilities/fs/writeTextFile"),
            Some(&Value::Bool(false))
        );
        assert_eq!(
            wire.pointer("/clientCapabilities/terminal"),
            Some(&Value::Bool(false))
        );
        assert_eq!(wire["_meta"]["clientIdentifier"], "umadev");
        assert_eq!(
            wire["clientCapabilities"]["_meta"]["x.ai/incrementalBashOutput"],
            true
        );
        assert_eq!(
            wire["clientCapabilities"]["_meta"]["x.ai/bashOutputNoColor"],
            true
        );
    }

    #[test]
    fn grok_wire_budget_accepts_the_shared_twenty_mib_binary_attachment_contract() {
        let blocks = [8_usize, 8, 4]
            .into_iter()
            .map(|mib| crate::turn_input::PreparedBlock::File {
                attachment: crate::turn_input::PreparedAttachment {
                    canonical_path: fixture_path("binary.dat"),
                    bytes: vec![0; mib * 1024 * 1024],
                    media_type: "application/octet-stream".to_string(),
                },
                mode: umadev_runtime::FileInputMode::NativeOnly,
            })
            .collect();
        let prepared = crate::turn_input::PreparedTurnInput { blocks };
        let (prompt, delivery) = acp_content_blocks(
            &prepared,
            NegotiatedCapabilities {
                embedded_context: true,
                ..NegotiatedCapabilities::default()
            },
            AcpVendor::Grok,
        )
        .unwrap();
        assert_eq!(delivery, vec![InputDelivery::Native; 3]);
        assert!(encoded_prompt_content_bytes(&prompt).unwrap() < MAX_PROMPT_CONTENT_BYTES);
    }

    #[test]
    fn grok_wire_budget_attributes_json_expansion_to_the_exact_file_block() {
        let attachment = || crate::turn_input::PreparedAttachment {
            canonical_path: fixture_path("control.txt"),
            // JSON encodes each control byte as six ASCII bytes. Two source
            // files remain below the shared 20 MiB input limit but cross the
            // Grok wire limit only when the second block is appended.
            bytes: vec![1; 6 * 1024 * 1024],
            media_type: "text/plain; charset=utf-8".to_string(),
        };
        let prepared = crate::turn_input::PreparedTurnInput {
            blocks: vec![
                crate::turn_input::PreparedBlock::File {
                    attachment: attachment(),
                    mode: umadev_runtime::FileInputMode::NativeOnly,
                },
                crate::turn_input::PreparedBlock::File {
                    attachment: attachment(),
                    mode: umadev_runtime::FileInputMode::NativeOnly,
                },
            ],
        };
        let error = acp_content_blocks(
            &prepared,
            NegotiatedCapabilities {
                embedded_context: true,
                ..NegotiatedCapabilities::default()
            },
            AcpVendor::Grok,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SessionError::InputInvalid {
                index: 1,
                kind: TurnInputBlockKind::File,
                ..
            }
        ));
    }

    #[test]
    fn fake_acp_published_grok_contract_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_acp_published_grok_contract_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        // Terminate libtest's `test <name> ... ` prefix before emitting JSON.
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            let response = match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    assert_eq!(frame["params"]["_meta"]["clientIdentifier"], "umadev");
                    assert_eq!(frame["params"]["clientCapabilities"]["terminal"], false);
                    Some(json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{
                                "loadSession":true,
                                "promptCapabilities":{"embeddedContext":true}
                            },
                            "_meta":{"grokShell":true,"agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION}
                        }
                    }))
                }
                Some("session/new") => {
                    assert_eq!(frame["params"]["_meta"]["rules"], "native-rules");
                    assert_eq!(frame["params"]["_meta"]["modelId"], "requested-model");
                    assert_eq!(frame["params"]["_meta"]["clientIdentifier"], "umadev");
                    std::fs::write("grok-new-meta-observed", b"ok").unwrap();
                    Some(json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "sessionId":"grok-source-session",
                            "models":{"currentModelId":"resolved-model","availableModels":[]}
                        }
                    }))
                }
                Some("session/set_mode") => {
                    assert_eq!(frame["params"]["sessionId"], "grok-source-session");
                    assert_eq!(frame["params"]["modeId"], "plan");
                    std::fs::write("grok-set-mode-observed", b"ok").unwrap();
                    Some(json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}))
                }
                Some("session/prompt") => {
                    assert_eq!(frame["params"]["_meta"]["mode"], "plan");
                    assert_eq!(frame["params"]["_meta"]["clientIdentifier"], "umadev");
                    assert!(frame["params"]["_meta"]["promptId"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("umadev-")));
                    assert_eq!(frame["params"]["prompt"][0]["text"], "build safely");
                    std::fs::write("grok-prompt-meta-observed", b"ok").unwrap();
                    Some(json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "stopReason":"end_turn",
                            "_meta":{
                                "inputTokens":12,"outputTokens":7,
                                "cachedReadTokens":8,"reasoningTokens":3
                            }
                        }
                    }))
                }
                Some("_x.ai/session/close") => {
                    assert_eq!(frame["params"]["sessionId"], "grok-source-session");
                    std::fs::write("grok-private-close-observed", b"ok").unwrap();
                    Some(json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"result":{"success":true}}
                    }))
                }
                _ => None,
            };
            if let Some(response) = response {
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
            }
        }
        // The pinned Grok source keeps a little over two seconds of cleanup
        // work after ACP stdin EOF. Model that tail so parent shutdown budgets
        // cannot regress to killing an otherwise healthy official process.
        std::thread::sleep(Duration::from_millis(2_200));
        std::fs::write("grok-eof-grace-observed", b"ok").unwrap();
    }

    #[tokio::test]
    async fn published_grok_fixture_drives_native_rules_model_plan_and_usage() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_published_grok_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args_and_firmware(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "requested-model",
            BasePermissionProfile::Plan,
            None,
            Some("native-rules".to_string()),
        )
        .await
        .unwrap();
        assert!(session.negotiated.grok_source_contract);
        assert!(session.negotiated.image);
        assert_eq!(
            session.capabilities().subagents,
            SubagentVisibility::Lifecycle
        );
        assert_eq!(
            session.capabilities().steer,
            SteerSemantics::SameTurnOrImmediateNext
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(
                SessionStateUpdate::ModelCatalogReplaced {
                    ref current_model_id,
                    ..
                }
            )) if current_model_id == "resolved-model"
        ));
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::SessionModel("resolved-model".to_string()))
        );
        session.send_turn("build safely".to_string()).await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), session.next_event())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event,
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }
        );
        session.end().await.unwrap();
        for marker in [
            "grok-new-meta-observed",
            "grok-set-mode-observed",
            "grok-prompt-meta-observed",
            "grok-private-close-observed",
            "grok-eof-grace-observed",
        ] {
            assert_eq!(std::fs::read(workspace.path().join(marker)).unwrap(), b"ok");
        }
    }

    #[test]
    fn fake_grok_interject_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_grok_interject_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        let mut prompt_rpc_id = None;
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{
                                "protocolVersion":1,
                                "agentCapabilities":{"loadSession":true},
                                "_meta":{
                                    "grokShell":true,
                                    "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                                }
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/new") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{"sessionId":"interject-session"}
                        })
                    )
                    .unwrap();
                }
                Some("session/set_mode") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{}
                        })
                    )
                    .unwrap();
                }
                Some("session/prompt") => {
                    prompt_rpc_id = frame.get("id").cloned();
                }
                Some("_x.ai/interject") => {
                    assert_eq!(frame["params"]["sessionId"], "interject-session");
                    assert_eq!(frame["params"]["text"], "look at this");
                    assert!(frame["params"]["interjectionId"]
                        .as_str()
                        .is_some_and(|id| id.starts_with("umadev-")));
                    assert_eq!(frame["params"]["content"][0]["text"], "look at this");
                    assert_eq!(frame["params"]["content"][1]["type"], "image");
                    assert_eq!(frame["params"]["content"][1]["mimeType"], "image/png");
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{"result":{"status":"queued"}}
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":prompt_rpc_id.take().unwrap(),
                            "result":{"stopReason":"end_turn"}
                        })
                    )
                    .unwrap();
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn grok_interject_uses_private_wire_and_structured_image_ack() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let image = workspace.path().join("steer.png");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_interject_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        assert!(session
            .capabilities()
            .supports(SessionCapability::MidTurnSteer));
        assert_eq!(
            session.capabilities().steer,
            SteerSemantics::SameTurnOrImmediateNext
        );
        session.send_turn("begin".to_string()).await.unwrap();
        let report = session
            .steer_input(TurnInput::new(vec![
                umadev_runtime::TurnInputBlock::Text {
                    text: "look at this".to_string(),
                },
                umadev_runtime::TurnInputBlock::Image { path: image },
            ]))
            .await
            .unwrap();
        assert_eq!(report.receipt, DeliveryReceiptStage::ProtocolAcknowledged);
        assert_eq!(report.blocks.len(), 2);
        assert!(report.encoded_bytes.is_some_and(|bytes| bytes > 0));
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            })
        ));
        session.end().await.unwrap();
    }

    fn emit_durable_fixture(stdout: &mut std::io::Stdout, value: impl Into<Value>) {
        let value = value.into();
        writeln!(stdout, "{value}").unwrap();
    }

    #[derive(Default)]
    struct DurableFixtureState {
        first_rpc_id: Option<Value>,
        first_prompt_id: Option<String>,
        prompt_count: u8,
    }

    impl DurableFixtureState {
        fn handle_prompt(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
            self.prompt_count += 1;
            let prompt_id = frame["params"]["_meta"]["promptId"]
                .as_str()
                .unwrap()
                .to_string();
            if self.prompt_count == 1 {
                self.first_rpc_id = frame.get("id").cloned();
                self.first_prompt_id = Some(prompt_id.clone());
                emit_durable_fixture(
                    stdout,
                    json!({
                        "jsonrpc":"2.0", "method":"session/update",
                        "params":{"sessionId":"durable-session","update":{
                            "sessionUpdate":"agent_message_chunk",
                            "content":{"type":"text","text":"first"}
                        }}
                    }),
                );
                emit_durable_fixture(
                    stdout,
                    json!({
                        "jsonrpc":"2.0", "method":"_x.ai/session/update",
                        "params":{"sessionId":"durable-session","update":{
                            "sessionUpdate":"turn_completed", "prompt_id":prompt_id,
                            "stop_reason":"end_turn",
                            "usage":{"inputTokens":21,"outputTokens":8,
                                "totalTokens":29,"cachedReadTokens":0,
                                "reasoningTokens":0,"modelCalls":1,"numTurns":1}
                        }}
                    }),
                );
            } else {
                self.emit_second_prompt(frame, stdout);
            }
        }

        fn emit_second_prompt(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
            for value in [
                json!({
                    "jsonrpc":"2.0", "id":self.first_rpc_id.take().unwrap(),
                    "result":{"stopReason":"end_turn"}
                }),
                json!({
                    "jsonrpc":"2.0", "method":"_x.ai/session/update",
                    "params":{"sessionId":"durable-session","update":{
                        "sessionUpdate":"turn_completed",
                        "prompt_id":self.first_prompt_id.take().unwrap(),
                        "stop_reason":"error", "agent_result":"stale terminal"
                    }}
                }),
                json!({
                    "jsonrpc":"2.0", "method":"session/update",
                    "params":{"sessionId":"durable-session","update":{
                        "sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"second"}
                    }}
                }),
                json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"stopReason":"end_turn"}
                }),
            ] {
                emit_durable_fixture(stdout, value);
            }
        }
    }

    #[test]
    fn fake_grok_durable_turn_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_grok_durable_turn_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        let mut state = DurableFixtureState::default();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => emit_durable_fixture(
                    &mut stdout,
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},
                            "_meta":{"grokShell":true,
                                "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION}}
                    }),
                ),
                Some("session/new") => emit_durable_fixture(
                    &mut stdout,
                    json!({"jsonrpc":"2.0", "id":frame["id"],
                        "result":{"sessionId":"durable-session"}}),
                ),
                Some("session/set_mode") => emit_durable_fixture(
                    &mut stdout,
                    json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}),
                ),
                Some("session/prompt") => state.handle_prompt(&frame, &mut stdout),
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn durable_turn_completion_is_correlated_first_wins_and_allows_next_turn() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_durable_turn_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        session.send_turn("one".to_string()).await.unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TextDelta("first".to_string()))
        );
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: Some(Usage {
                    model_calls: 1,
                    num_turns: 1,
                    ..Usage::exact(21, 8)
                }),
            })
        );

        session.send_turn("two".to_string()).await.unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TextDelta("second".to_string()))
        );
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            })
        );
        session.end().await.unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fake_grok_nested_route_convergence_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args()
                .any(|arg| arg.ends_with("fake_grok_nested_route_convergence_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        let mut prompt_count = 0_u8;
        let mut current_prompt_id = String::new();
        let mut child_answered = false;
        let mut forged_rejected = false;
        let mut child_permission_answered = false;
        let mut child_plan_answered = false;
        let mut child_interactions_sent = false;
        let mut nested_lifecycle_sent = false;

        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{
                                "protocolVersion":1,
                                "agentCapabilities":{"loadSession":true},
                                "_meta":{
                                    "grokShell":true,
                                    "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                                }
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/new") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{"sessionId":"route-root"}
                        })
                    )
                    .unwrap();
                }
                Some("session/set_mode") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}})
                    )
                    .unwrap();
                }
                Some("session/prompt") => {
                    prompt_count = prompt_count.saturating_add(1);
                    current_prompt_id = frame["params"]["_meta"]["promptId"]
                        .as_str()
                        .unwrap()
                        .to_string();
                    let (child, subagent, event) = if prompt_count == 1 {
                        ("child-a", "agent-a", "spawn-a")
                    } else {
                        ("child-cancel", "agent-cancel", "spawn-cancel")
                    };
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"_x.ai/session/update",
                            "params":{
                                "sessionId":"route-root", "_meta":{"eventId":event},
                                "update":{
                                    "sessionUpdate":"subagent_spawned",
                                    "subagent_id":subagent,
                                    "parent_session_id":"route-root",
                                    "parent_prompt_id":current_prompt_id,
                                    "child_session_id":child,
                                    "subagent_type":"general-purpose",
                                    "description":"nested route fixture"
                                }
                            }
                        })
                    )
                    .unwrap();
                    let request_id = if prompt_count == 1 {
                        "ask-child"
                    } else {
                        "ask-cancel"
                    };
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":request_id,
                            "method":"_x.ai/ask_user_question",
                            "params":{
                                "sessionId":child,
                                "toolCallId":format!("tool-{request_id}"),
                                "mode":"default",
                                "questions":[{
                                    "id":"q1", "question":"Continue?", "multiSelect":false,
                                    "options":[{"label":"Yes","description":"Continue"}]
                                }]
                            }
                        })
                    )
                    .unwrap();
                    if prompt_count == 1 {
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "id":"ask-forged",
                                "method":"_x.ai/ask_user_question",
                                "params":{
                                    "sessionId":"forged-child", "toolCallId":"tool-forged",
                                    "mode":"default",
                                    "questions":[{
                                        "id":"q1", "question":"Forged?", "multiSelect":false,
                                        "options":[{"label":"No","description":"Reject"}]
                                    }]
                                }
                            })
                        )
                        .unwrap();
                    }
                }
                Some("session/cancel") => {
                    assert_eq!(frame["params"]["sessionId"], "route-root");
                    assert_eq!(frame["params"]["_meta"]["cancelSubagents"], true);
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"_x.ai/session/update",
                            "params":{"sessionId":"route-root","update":{
                                "sessionUpdate":"turn_completed",
                                "prompt_id":current_prompt_id,
                                "stop_reason":"cancelled"
                            }}
                        })
                    )
                    .unwrap();
                }
                Some("_x.ai/session/close") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}})
                    )
                    .unwrap();
                    stdout.flush().unwrap();
                    break;
                }
                None => {
                    match frame.get("id").and_then(Value::as_str) {
                        Some("ask-child") => child_answered = true,
                        Some("ask-forged") => {
                            assert_eq!(frame["error"]["code"], -32_602);
                            forged_rejected = true;
                            std::fs::write("forged-child-rejected", b"ok").unwrap();
                        }
                        Some("permission-child") => child_permission_answered = true,
                        Some("plan-child") => child_plan_answered = true,
                        Some("ask-grandchild") => {
                            writeln!(
                                stdout,
                                "{}",
                                json!({
                                    "jsonrpc":"2.0", "method":"_x.ai/session/update",
                                    "params":{
                                        "sessionId":"child-a", "_meta":{"eventId":"finish-b"},
                                        "update":{
                                            "sessionUpdate":"subagent_finished",
                                            "subagent_id":"agent-b",
                                            "child_session_id":"grandchild-b",
                                            "status":"completed",
                                            "tool_calls":1,
                                            "turns":1,
                                            "duration_ms":20,
                                            "tokens_used":4,
                                            "will_wake":true
                                        }
                                    }
                                })
                            )
                            .unwrap();
                            writeln!(
                                stdout,
                                "{}",
                                json!({
                                    "jsonrpc":"2.0", "method":"session/update",
                                    "params":{"sessionId":"route-root","update":{
                                        "sessionUpdate":"agent_message_chunk",
                                        "content":{"type":"text","text":"synthetic-final"}
                                    }}
                                })
                            )
                            .unwrap();
                            writeln!(
                                stdout,
                                "{}",
                                json!({
                                    "jsonrpc":"2.0", "method":"_x.ai/session/update",
                                    "params":{"sessionId":"route-root","update":{
                                        "sessionUpdate":"turn_completed",
                                        "prompt_id":"subagent-completed-agent-b",
                                        "stop_reason":"end_turn",
                                        "usage":{"inputTokens":5,"outputTokens":3,
                                            "totalTokens":8,"cachedReadTokens":0,
                                            "reasoningTokens":0,"modelCalls":1,"numTurns":1}
                                    }}
                                })
                            )
                            .unwrap();
                        }
                        Some("ask-cancel") => {
                            assert_eq!(frame["result"]["outcome"], "cancelled");
                            std::fs::write("child-interaction-cancelled", b"ok").unwrap();
                        }
                        _ => {}
                    }

                    if child_answered && forged_rejected && !child_interactions_sent {
                        child_interactions_sent = true;
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "id":"permission-child",
                                "method":"session/request_permission",
                                "params":{
                                    "sessionId":"child-a",
                                    "toolCall":{"toolCallId":"tool-permission","kind":"write","title":"Write","rawInput":{"path":"src/lib.rs"}},
                                    "options":[
                                        {"optionId":"allow-once","name":"Allow","kind":"allow_once"},
                                        {"optionId":"reject-once","name":"Reject","kind":"reject_once"}
                                    ]
                                }
                            })
                        )
                        .unwrap();
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "id":"plan-child",
                                "method":"_x.ai/exit_plan_mode",
                                "params":{
                                    "sessionId":"child-a", "toolCallId":"tool-plan",
                                    "planContent":"Implement safely", "message":"Proceed?"
                                }
                            })
                        )
                        .unwrap();
                    }

                    if child_permission_answered && child_plan_answered && !nested_lifecycle_sent {
                        nested_lifecycle_sent = true;
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "method":"_x.ai/session/update",
                                "params":{
                                    "sessionId":"child-a", "_meta":{"eventId":"spawn-b"},
                                    "update":{
                                        "sessionUpdate":"subagent_spawned",
                                        "subagent_id":"agent-b",
                                        "parent_session_id":"child-a",
                                        "child_session_id":"grandchild-b",
                                        "subagent_type":"general-purpose",
                                        "description":"nested child"
                                    }
                                }
                            })
                        )
                        .unwrap();
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "method":"_x.ai/session/update",
                                "params":{"sessionId":"route-root","update":{
                                    "sessionUpdate":"turn_completed",
                                    "prompt_id":current_prompt_id,
                                    "stop_reason":"end_turn",
                                    "usage":{"inputTokens":10,"outputTokens":2,
                                        "totalTokens":12,"cachedReadTokens":0,
                                        "reasoningTokens":0,"modelCalls":1,"numTurns":1}
                                }}
                            })
                        )
                        .unwrap();
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "method":"_x.ai/session/update",
                                "params":{
                                    "sessionId":"route-root", "_meta":{"eventId":"finish-a"},
                                    "update":{
                                        "sessionUpdate":"subagent_finished",
                                        "subagent_id":"agent-a", "child_session_id":"child-a",
                                        "status":"completed", "tool_calls":1, "turns":1,
                                        "duration_ms":10, "will_wake":false
                                    }
                                }
                            })
                        )
                        .unwrap();
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "id":"ask-grandchild",
                                "method":"_x.ai/ask_user_question",
                                "params":{
                                    "sessionId":"grandchild-b", "toolCallId":"tool-grandchild",
                                    "mode":"default",
                                    "questions":[{
                                        "id":"q1", "question":"Finish?", "multiSelect":false,
                                        "options":[{"label":"Yes","description":"Finish"}]
                                    }]
                                }
                            })
                        )
                        .unwrap();
                    }
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn grok_nested_routes_interactions_wake_and_cancel_converge_once() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_nested_route_convergence_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();

        session.send_turn("coordinate".to_string()).await.unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started {
                    id: "agent-a".to_string()
                }
            ))
        );
        let ask_id = match session.next_event().await {
            Some(SessionEvent::HostRequest {
                req_id,
                request: HostRequest::UserInput { .. },
            }) => req_id,
            other => panic!("expected child question, got {other:?}"),
        };
        session
            .respond_host(
                &ask_id,
                HostResponse::UserInputOutcome {
                    outcome: HostUserInputOutcome::Accepted {
                        answers: vec![HostAnswer {
                            question_id: "q1".to_string(),
                            values: vec!["Yes".to_string()],
                        }],
                        annotations: Vec::new(),
                    },
                },
            )
            .await
            .unwrap();

        let permission_id = match session.next_event().await {
            Some(SessionEvent::HostRequest {
                req_id,
                request: HostRequest::Approval { .. },
            }) => req_id,
            other => panic!("expected child permission, got {other:?}"),
        };
        session
            .respond(&permission_id, ApprovalDecision::Allow)
            .await
            .unwrap();
        let plan_id = match session.next_event().await {
            Some(SessionEvent::HostRequest {
                req_id,
                request: HostRequest::PlanConfirmation { .. },
            }) => req_id,
            other => panic!("expected child plan, got {other:?}"),
        };
        session
            .respond_host(
                &plan_id,
                HostResponse::PlanOutcome {
                    outcome: HostPlanOutcome::Approved,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started {
                    id: "agent-b".to_string()
                }
            ))
        );
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Finished {
                    id: "agent-a".to_string()
                }
            ))
        );
        let grandchild_ask = match session.next_event().await {
            Some(SessionEvent::HostRequest {
                req_id,
                request: HostRequest::UserInput { .. },
            }) => req_id,
            other => panic!("expected reparented grandchild question, got {other:?}"),
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), session.next_event())
                .await
                .is_err(),
            "root terminal must remain buffered while the descendant is live"
        );
        session
            .respond_host(
                &grandchild_ask,
                HostResponse::UserInputOutcome {
                    outcome: HostUserInputOutcome::Accepted {
                        answers: vec![HostAnswer {
                            question_id: "q1".to_string(),
                            values: vec!["Yes".to_string()],
                        }],
                        annotations: Vec::new(),
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Finished {
                    id: "agent-b".to_string()
                }
            ))
        );
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TextDelta("synthetic-final".to_string()))
        );
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: Some(Usage {
                    model_calls: 2,
                    num_turns: 2,
                    ..Usage::exact(15, 5)
                })
            })
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), session.next_event())
                .await
                .is_err(),
            "root and synthetic terminal rails must converge exactly once"
        );
        assert_eq!(
            std::fs::read(workspace.path().join("forged-child-rejected")).unwrap(),
            b"ok"
        );

        session.send_turn("cancel child".to_string()).await.unwrap();
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(
                BackgroundTaskSignal::Started { ref id }
            )) if id == "agent-cancel"
        ));
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::HostRequest {
                request: HostRequest::UserInput { .. },
                ..
            })
        ));
        session.interrupt().await.unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(BackgroundTaskSignal::Live {
                agent_ids: Vec::new()
            }))
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !workspace
                .path()
                .join("child-interaction-cancelled")
                .exists()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child interaction must receive a typed cancellation");
        session.end().await.unwrap();
    }

    #[test]
    fn fake_acp_close_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_acp_close_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let response = match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{
                        "protocolVersion":1,
                        "agentCapabilities":{"sessionCapabilities":{"close":{}}}
                    }
                })),
                Some("session/new") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"sessionId":"close-me"}
                })),
                Some("session/close") => {
                    assert_eq!(
                        frame.pointer("/params/sessionId").and_then(Value::as_str),
                        Some("close-me")
                    );
                    std::fs::write("close-observed", b"closed").unwrap();
                    Some(json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}))
                }
                _ => None,
            };
            if let Some(response) = response {
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn end_negotiates_session_close_before_reaping_the_acp_process() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_close_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        assert!(session.negotiated.close);
        session.end().await.unwrap();
        assert_eq!(
            std::fs::read(workspace.path().join("close-observed")).unwrap(),
            b"closed"
        );
    }

    #[tokio::test]
    async fn structured_prompt_requires_exact_negotiated_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("截图 空格.png");
        let file = dir.path().join("资料 中文.txt");
        std::fs::write(&image, b"\x89PNG\r\n\x1a\nfixture").unwrap();
        std::fs::write(&file, "context").unwrap();
        let prepared = crate::turn_input::prepare(TurnInput::new(vec![
            umadev_runtime::TurnInputBlock::Text {
                text: "before".into(),
            },
            umadev_runtime::TurnInputBlock::Image {
                path: image.clone(),
            },
            umadev_runtime::TurnInputBlock::File {
                path: file,
                mode: umadev_runtime::FileInputMode::NativeOnly,
            },
        ]))
        .await
        .unwrap();
        let error = acp_content_blocks(
            &prepared,
            NegotiatedCapabilities::default(),
            AcpVendor::Grok,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SessionError::InputUnsupported { index: 1, .. }
        ));

        let (blocks, deliveries) = acp_content_blocks(
            &prepared,
            NegotiatedCapabilities {
                image: true,
                embedded_context: true,
                ..NegotiatedCapabilities::default()
            },
            AcpVendor::Grok,
        )
        .unwrap();
        let wire = serde_json::to_value(blocks).unwrap();
        assert_eq!(wire[0]["type"], "text");
        assert_eq!(wire[1]["type"], "image");
        assert_eq!(wire[1]["mimeType"], "image/png");
        assert_eq!(wire[2]["type"], "resource");
        assert!(wire[2]["resource"]["uri"]
            .as_str()
            .unwrap()
            .starts_with("file:"));
        assert_eq!(deliveries, vec![InputDelivery::Native; 3]);

        let image_first = crate::turn_input::prepare(TurnInput::new(vec![
            umadev_runtime::TurnInputBlock::Image { path: image },
        ]))
        .await
        .unwrap();
        let (blocks, _) = acp_content_blocks(
            &image_first,
            NegotiatedCapabilities {
                image: true,
                ..NegotiatedCapabilities::default()
            },
            AcpVendor::Grok,
        )
        .unwrap();
        let wire = serde_json::to_value(blocks).unwrap();
        assert_eq!(wire[0]["type"], "image");
    }

    #[test]
    fn kimi_embedded_resources_are_utf8_text_only_without_false_blob_delivery() {
        let capabilities = NegotiatedCapabilities {
            embedded_context: true,
            ..NegotiatedCapabilities::default()
        };
        let text = crate::turn_input::PreparedTurnInput {
            blocks: vec![crate::turn_input::PreparedBlock::File {
                attachment: crate::turn_input::PreparedAttachment {
                    canonical_path: fixture_path("context.txt"),
                    bytes: "Kimi 原生文本上下文".as_bytes().to_vec(),
                    media_type: "text/plain; charset=utf-8".to_string(),
                },
                mode: umadev_runtime::FileInputMode::NativeOnly,
            }],
        };
        let (blocks, deliveries) =
            acp_content_blocks(&text, capabilities, AcpVendor::Kimi).unwrap();
        let wire = serde_json::to_value(blocks).unwrap();
        assert_eq!(wire[0]["type"], "resource");
        assert_eq!(wire[0]["resource"]["text"], "Kimi 原生文本上下文");
        assert!(wire[0]["resource"].get("blob").is_none());
        assert_eq!(deliveries, vec![InputDelivery::Native]);

        let binary = crate::turn_input::PreparedTurnInput {
            blocks: vec![crate::turn_input::PreparedBlock::File {
                attachment: crate::turn_input::PreparedAttachment {
                    canonical_path: fixture_path("archive.bin"),
                    bytes: vec![0, 0xff, 1, 2],
                    media_type: "application/octet-stream".to_string(),
                },
                mode: umadev_runtime::FileInputMode::NativeOnly,
            }],
        };
        let error = acp_content_blocks(&binary, capabilities, AcpVendor::Kimi).unwrap_err();
        assert!(matches!(
            &error,
            SessionError::InputUnsupported {
                index: 0,
                kind: TurnInputBlockKind::File,
                reason,
            } if reason.contains("official adapter drops binary blob resources")
        ));

        let (blocks, deliveries) =
            acp_content_blocks(&binary, capabilities, AcpVendor::Grok).unwrap();
        let wire = serde_json::to_value(blocks).unwrap();
        assert!(wire[0]["resource"]["blob"].is_string());
        assert_eq!(deliveries, vec![InputDelivery::Native]);
    }

    #[test]
    fn session_mode_selection_preserves_each_vendor_permission_boundary() {
        let no_modes = json!({});
        assert_eq!(
            select_session_mode(
                &no_modes,
                AcpVendor::Grok,
                BasePermissionProfile::Plan,
                true,
            ),
            Some("plan".to_string())
        );
        assert_eq!(
            select_session_mode(
                &no_modes,
                AcpVendor::Grok,
                BasePermissionProfile::Auto,
                true,
            ),
            Some("default".to_string())
        );

        let grok = json!({"modes":{"availableModes":[
            {"id":"default"},{"id":"bypassPermissions"}
        ]}});
        assert_eq!(
            select_session_mode(
                &grok,
                AcpVendor::Grok,
                BasePermissionProfile::Guarded,
                false,
            ),
            Some("default".to_string())
        );
        assert_eq!(
            select_session_mode(&grok, AcpVendor::Grok, BasePermissionProfile::Auto, false,),
            Some("bypassPermissions".to_string())
        );

        let kimi = json!({"configOptions":[{
            "type":"select","id":"mode","currentValue":"default","options":[
                {"value":"default","name":"Default"},
                {"value":"plan","name":"Plan"},
                {"value":"auto","name":"Auto"},
                {"value":"yolo","name":"YOLO"}
            ]
        }]});
        assert_eq!(
            select_session_mode(&kimi, AcpVendor::Kimi, BasePermissionProfile::Plan, true,),
            Some("plan".to_string())
        );
        assert_eq!(
            select_session_mode(&kimi, AcpVendor::Kimi, BasePermissionProfile::Auto, true,),
            Some("default".to_string()),
            "UmaDev keeps the irreversible approval floor instead of selecting Kimi yolo"
        );
    }

    #[test]
    fn source_shaped_model_catalogs_are_strict_full_replacements() {
        let model_state = json!({
            "currentModelId":"grok-code-fast-1",
            "availableModels":[{
                "modelId":"grok-code-fast-1",
                "name":"Grok Code Fast",
                "description":"Fast coding model",
                "_meta":{
                    "totalContextTokens":262_144,
                    "agentType":"grok-build-plan",
                    "supportsReasoningEffort":true,
                    "reasoningEffort":"high",
                    "reasoningEfforts":[
                        "low",
                        {"id":"deep","value":"xhigh","label":"Deep","description":"Maximum","default":true},
                        {"value":"future-tier"}
                    ]
                }
            }]
        });
        let initialize = json!({"_meta":{"modelState":model_state.clone()}});
        let setup = json!({"models":model_state.clone()});
        let expected = parse_model_catalog(&model_state).unwrap();
        assert_eq!(
            parse_initialize_model_catalog(&initialize),
            Some(expected.clone())
        );
        assert_eq!(parse_setup_model_catalog(&setup), Some(expected.clone()));

        let kimi_setup = json!({"configOptions":[{
            "type":"select","id":"model","name":"Model","currentValue":"kimi-k2",
            "options":[
                {"value":"kimi-k2","name":"Kimi K2","description":"Coding model"},
                {"value":"kimi-k2-fast","name":"Kimi K2 Fast"}
            ]
        }]});
        let SessionStateUpdate::ModelCatalogReplaced {
            current_model_id: kimi_current,
            available_models: kimi_models,
        } = parse_setup_model_catalog(&kimi_setup).unwrap()
        else {
            panic!("expected Kimi model config catalog");
        };
        assert_eq!(kimi_current, "kimi-k2");
        assert_eq!(kimi_models.len(), 2);
        assert_eq!(kimi_models[0].description.as_deref(), Some("Coding model"));
        assert!(!kimi_models[0].supports_reasoning_effort);

        let SessionStateUpdate::ModelCatalogReplaced {
            current_model_id,
            available_models,
        } = expected
        else {
            panic!("expected a model catalog replacement");
        };
        assert_eq!(current_model_id, "grok-code-fast-1");
        assert_eq!(available_models.len(), 1);
        let model = &available_models[0];
        assert_eq!(model.total_context_tokens, Some(262_144));
        assert_eq!(model.agent_type.as_deref(), Some("grok-build-plan"));
        assert_eq!(model.reasoning_effort, Some(SessionReasoningEffort::High));
        assert_eq!(model.reasoning_efforts.len(), 2);
        assert_eq!(model.reasoning_efforts[0].id, "low");
        assert_eq!(
            model.reasoning_efforts[1].value,
            SessionReasoningEffort::Xhigh
        );

        let empty = parse_model_catalog(&json!({
            "currentModelId":"grok-code-fast-1", "availableModels":[]
        }));
        assert!(matches!(
            empty,
            Some(SessionStateUpdate::ModelCatalogReplaced { available_models, .. })
                if available_models.is_empty()
        ));
        assert!(parse_model_catalog(&json!({
            "currentModelId":" future ", "availableModels":[]
        }))
        .is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn source_shaped_command_and_mode_updates_replace_state_without_rendering() {
        let initialize = json!({"_meta":{"availableCommands":[
            {"name":"compact","description":"Compact context","input":{"hint":"focus"}},
            {"name":"skill","description":"Run skill","input":null,
             "_meta":{"scope":"project","path":".agents/skills/review.md"}}
        ]}});
        let init_update = parse_initialize_command_catalog(&initialize).unwrap();
        let SessionStateUpdate::CommandCatalogReplaced { commands, tools } = init_update else {
            panic!("expected command catalog");
        };
        assert_eq!(commands.len(), 2);
        assert!(tools.is_empty());
        assert_eq!(commands[0].input_hint.as_deref(), Some("focus"));
        assert_eq!(commands[1].scope.as_deref(), Some("project"));

        let update = json!({
            "sessionUpdate":"available_commands_update",
            "availableCommands":[],
            "_meta":{"tools":["read_file","bash"]}
        });
        assert!(matches!(
            parse_standard_command_catalog(&update),
            Some(SessionStateUpdate::CommandCatalogReplaced { commands, tools })
                if commands.is_empty() && tools == ["read_file", "bash"]
        ));
        assert_eq!(
            parse_current_mode_update(&json!({"currentModeId":"ask"})),
            Some(SessionStateUpdate::ModeChanged {
                mode: SessionMode::Ask
            })
        );
        assert!(parse_current_mode_update(&json!({"currentModeId":"bypassPermissions"})).is_none());

        let events = parse_config_option_state_events(&json!({"configOptions":[
            {"type":"select","id":"model","currentValue":"kimi-k2","options":[
                {"value":"kimi-k2","name":"Kimi K2"},
                {"value":"kimi-k2-fast","name":"Kimi K2 Fast"}
            ]},
            {"type":"select","id":"thinking","currentValue":"on","options":[
                {"value":"off","name":"Thinking Off"},
                {"value":"on","name":"Thinking On"}
            ]},
            {"type":"select","id":"mode","currentValue":"plan"}
        ]}));
        assert_eq!(
            events,
            vec![
                SessionEvent::StateUpdate(SessionStateUpdate::ModelCatalogReplaced {
                    current_model_id: "kimi-k2".to_string(),
                    available_models: vec![
                        SessionModelInfo {
                            model_id: "kimi-k2".to_string(),
                            name: "Kimi K2".to_string(),
                            description: None,
                            total_context_tokens: None,
                            agent_type: None,
                            supports_reasoning_effort: false,
                            reasoning_effort: None,
                            reasoning_efforts: Vec::new(),
                        },
                        SessionModelInfo {
                            model_id: "kimi-k2-fast".to_string(),
                            name: "Kimi K2 Fast".to_string(),
                            description: None,
                            total_context_tokens: None,
                            agent_type: None,
                            supports_reasoning_effort: false,
                            reasoning_effort: None,
                            reasoning_efforts: Vec::new(),
                        },
                    ],
                }),
                SessionEvent::SessionModel("kimi-k2".to_string()),
                SessionEvent::StateUpdate(SessionStateUpdate::ThinkingChanged {
                    enabled: Some(true),
                    can_enable: true,
                    can_disable: true,
                }),
                SessionEvent::StateUpdate(SessionStateUpdate::ModeChanged {
                    mode: SessionMode::Plan,
                }),
            ]
        );
        assert_eq!(
            parse_config_option_state_events(&json!({"configOptions":[
                {"type":"select","id":"mode","currentValue":"yolo"}
            ]})),
            vec![SessionEvent::StateUpdate(
                SessionStateUpdate::ThinkingChanged {
                    enabled: None,
                    can_enable: false,
                    can_disable: false,
                }
            )],
            "a full snapshot clears a removed thinking control without reclassifying an unmapped authority mode"
        );

        assert_eq!(
            parse_config_option_state_events(&json!({"configOptions":[
                {"type":"select","id":"thinking","currentValue":"on","options":[
                    {"value":"on","name":"Thinking On"}
                ]}
            ]})),
            vec![SessionEvent::StateUpdate(
                SessionStateUpdate::ThinkingChanged {
                    enabled: Some(true),
                    can_enable: true,
                    can_disable: false,
                }
            )],
            "an always-thinking model stays visibly locked on"
        );
    }

    #[test]
    fn published_kimi_plan_update_is_a_bounded_full_replacement() {
        let update = json!({
            "sessionUpdate":"plan",
            "entries":[
                {"content":"Inspect the repository","priority":"medium","status":"completed"},
                {"content":"Implement the fix","priority":"high","status":"in_progress"},
                {"content":"Run verification","priority":"low","status":"pending"}
            ]
        });
        assert_eq!(
            parse_standard_plan(&update),
            Some(SessionStateUpdate::PlanReplaced {
                entries: vec![
                    SessionPlanEntry {
                        content: "Inspect the repository".to_string(),
                        priority: SessionPlanEntryPriority::Medium,
                        status: SessionPlanEntryStatus::Completed,
                    },
                    SessionPlanEntry {
                        content: "Implement the fix".to_string(),
                        priority: SessionPlanEntryPriority::High,
                        status: SessionPlanEntryStatus::InProgress,
                    },
                    SessionPlanEntry {
                        content: "Run verification".to_string(),
                        priority: SessionPlanEntryPriority::Low,
                        status: SessionPlanEntryStatus::Pending,
                    },
                ],
            })
        );

        let mut tools = ToolState::default();
        assert!(matches!(
            parse_session_update(&json!({"update":update}), &mut tools).as_slice(),
            [SessionEvent::StateUpdate(SessionStateUpdate::PlanReplaced { entries })]
                if entries.len() == 3
        ));
        assert_eq!(
            parse_standard_plan(&json!({"sessionUpdate":"plan","entries":[]})),
            Some(SessionStateUpdate::PlanReplaced {
                entries: Vec::new()
            })
        );
        for malformed in [
            json!({"sessionUpdate":"plan","entries":[{"content":"","priority":"medium","status":"pending"}]}),
            json!({"sessionUpdate":"plan","entries":[{"content":"x","priority":"urgent","status":"pending"}]}),
            json!({"sessionUpdate":"plan","entries":[{"content":"x","priority":"medium","status":"running"}]}),
        ] {
            assert!(parse_standard_plan(&malformed).is_none());
        }
    }

    #[test]
    fn source_gated_private_model_transitions_preserve_exact_semantics() {
        assert_eq!(
            parse_grok_model_state_events(&json!({"update":{
                "sessionUpdate":"model_changed",
                "model_id":"grok-4.5",
                "reasoning_effort":"xhigh"
            }})),
            vec![
                SessionEvent::StateUpdate(SessionStateUpdate::ModelChanged {
                    model_id: "grok-4.5".to_string(),
                    reasoning_effort: Some(SessionReasoningEffort::Xhigh),
                }),
                SessionEvent::SessionModel("grok-4.5".to_string()),
            ]
        );
        assert!(parse_grok_model_state_events(&json!({"update":{
            "sessionUpdate":"model_changed",
            "model_id":"grok-4.5",
            "reasoning_effort":"max"
        }}))
        .is_empty());
        assert_eq!(
            parse_grok_model_state_events(&json!({"update":{
                "sessionUpdate":"model_auto_switched",
                "previous_model_id":"retired-model",
                "new_model_id":"grok-code-fast-1",
                "reason":"persisted model is unavailable"
            }})),
            vec![
                SessionEvent::StateUpdate(SessionStateUpdate::ModelAutoSwitched {
                    previous_model_id: "retired-model".to_string(),
                    new_model_id: "grok-code-fast-1".to_string(),
                    reason: "persisted model is unavailable".to_string(),
                }),
                SessionEvent::SessionModel("grok-code-fast-1".to_string()),
            ]
        );
    }

    #[test]
    fn replay_suppresses_presentation_but_retains_latest_typed_state() {
        let mut replay = ReplaySessionStateAccumulator::default();
        replay.remember_event(SessionEvent::TextDelta("old transcript".to_string()));
        replay.remember_event(SessionEvent::StateUpdate(
            SessionStateUpdate::CommandCatalogReplaced {
                commands: Vec::new(),
                tools: vec!["old".to_string()],
            },
        ));
        replay.remember_event(SessionEvent::StateUpdate(
            SessionStateUpdate::CommandCatalogReplaced {
                commands: Vec::new(),
                tools: vec!["new".to_string()],
            },
        ));
        replay.remember_event(SessionEvent::StateUpdate(
            SessionStateUpdate::ModelAutoSwitched {
                previous_model_id: "retired".to_string(),
                new_model_id: "current".to_string(),
                reason: "unavailable".to_string(),
            },
        ));
        replay.remember_event(SessionEvent::SessionModel("current".to_string()));
        replay.remember_event(SessionEvent::StateUpdate(SessionStateUpdate::ModeChanged {
            mode: SessionMode::Plan,
        }));
        replay.remember_event(SessionEvent::StateUpdate(
            SessionStateUpdate::ThinkingChanged {
                enabled: Some(false),
                can_enable: true,
                can_disable: true,
            },
        ));
        replay.remember_event(SessionEvent::StateUpdate(
            SessionStateUpdate::ThinkingChanged {
                enabled: Some(true),
                can_enable: true,
                can_disable: true,
            },
        ));
        replay.remember_event(SessionEvent::StateUpdate(
            SessionStateUpdate::PlanReplaced {
                entries: vec![SessionPlanEntry {
                    content: "latest base plan".to_string(),
                    priority: SessionPlanEntryPriority::Medium,
                    status: SessionPlanEntryStatus::InProgress,
                }],
            },
        ));

        let events = replay.take_events();
        assert!(!events
            .iter()
            .any(|event| matches!(event, SessionEvent::TextDelta(_))));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::StateUpdate(SessionStateUpdate::ModelAutoSwitched { reason, .. })
                if reason == "unavailable"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::StateUpdate(SessionStateUpdate::CommandCatalogReplaced { tools, .. })
                if tools == &["new"]
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::StateUpdate(SessionStateUpdate::ModeChanged {
                mode: SessionMode::Plan
            })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::StateUpdate(SessionStateUpdate::ThinkingChanged {
                enabled: Some(true),
                can_enable: true,
                can_disable: true,
            })
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::StateUpdate(SessionStateUpdate::PlanReplaced { entries })
                if entries.first().is_some_and(|entry| entry.content == "latest base plan")
        )));
    }

    #[tokio::test]
    async fn critical_events_survive_a_slow_consumer_after_a_large_display_burst() {
        let (event_tx, mut events) = mpsc::channel(EVENT_CHANNEL_CAP);
        for index in 0..(EVENT_CHANNEL_CAP + 64) {
            emit_event(
                &event_tx,
                SessionEvent::TextDelta(format!("decorative-{index}")),
            )
            .await;
        }

        let producer = tokio::spawn(async move {
            emit_event(
                &event_tx,
                SessionEvent::ToolCallCorrelated {
                    call_id: "burst-tool".to_string(),
                    name: "Bash".to_string(),
                    input: json!({"command":"cargo test"}),
                },
            )
            .await;
            emit_event(
                &event_tx,
                SessionEvent::ToolResultCorrelated {
                    call_id: "burst-tool".to_string(),
                    ok: true,
                    summary: "passed".to_string(),
                },
            )
            .await;
            emit_event(
                &event_tx,
                SessionEvent::SessionModel("grok-test-model".to_string()),
            )
            .await;
            emit_event(
                &event_tx,
                SessionEvent::TurnDone {
                    status: TurnStatus::Completed,
                    usage: None,
                },
            )
            .await;
        });

        tokio::task::yield_now().await;
        assert!(
            !producer.is_finished(),
            "critical delivery must backpressure while the bounded queue is full"
        );

        let mut saw_call = false;
        let mut saw_result = false;
        let mut saw_model = false;
        let mut saw_done = false;
        while !(saw_call && saw_result && saw_model && saw_done) {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("slow consumer should still receive reliable events")
                .expect("producer should keep the event channel open");
            match event {
                SessionEvent::ToolCallCorrelated { call_id, .. } => {
                    saw_call = call_id == "burst-tool";
                }
                SessionEvent::ToolResultCorrelated { call_id, ok, .. } => {
                    saw_result = call_id == "burst-tool" && ok;
                }
                SessionEvent::SessionModel(model) => saw_model = model == "grok-test-model",
                SessionEvent::TurnDone { status, .. } => {
                    saw_done = matches!(status, TurnStatus::Completed);
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::timeout(Duration::from_secs(2), producer)
            .await
            .expect("reliable producer should unblock once the consumer drains")
            .expect("reliable producer task should not panic");
    }

    #[tokio::test]
    async fn critical_delivery_returns_promptly_after_receiver_drop() {
        let (event_tx, events) = mpsc::channel(1);
        drop(events);

        tokio::time::timeout(Duration::from_millis(100), async {
            emit_event(
                &event_tx,
                SessionEvent::ToolCall {
                    name: "Bash".to_string(),
                    input: json!({"command":"cargo test"}),
                },
            )
            .await;
            emit_event(
                &event_tx,
                SessionEvent::ToolResult {
                    ok: false,
                    summary: "receiver gone".to_string(),
                },
            )
            .await;
            emit_event(
                &event_tx,
                SessionEvent::SessionModel("grok-test-model".to_string()),
            )
            .await;
            emit_event(
                &event_tx,
                SessionEvent::TurnDone {
                    status: TurnStatus::Failed("receiver gone".to_string()),
                    usage: None,
                },
            )
            .await;
        })
        .await
        .expect("a closed receiver must release every awaited critical send");
    }

    #[test]
    fn grok_auth_obeys_audited_default_then_safe_headless_fallbacks() {
        let init = json!({
            "authMethods":[
                {"id":"xai.api_key"}, {"id":"cached_token"}, {"id":"grok.com"}
            ],
            "_meta":{"defaultAuthMethodId":"cached_token"}
        });
        assert_eq!(safe_grok_auth_method(&init, true), Some("cached_token"));
        assert_eq!(safe_grok_auth_method(&init, false), Some("cached_token"));

        let api_key_default = json!({
            "authMethods":[{"id":"cached_token"},{"id":"xai.api_key"}],
            "_meta":{"defaultAuthMethodId":"xai.api_key"}
        });
        assert_eq!(
            safe_grok_auth_method(&api_key_default, true),
            Some("xai.api_key")
        );
        let api_key_only = json!({"authMethods":[{"id":"xai.api_key"}]});
        assert_eq!(
            safe_grok_auth_method(&api_key_only, true),
            Some("xai.api_key")
        );
        assert_eq!(
            safe_grok_auth_method(&json!({"authMethods":[]}), true),
            None
        );

        let interactive = json!({"authMethods":[{"id":"oauth"},{"id":"browser_login"}]});
        assert_eq!(safe_grok_auth_method(&interactive, true), None);
        assert_eq!(safe_grok_auth_method(&interactive, false), None);
    }

    #[test]
    fn grok_auth_gate_is_visible_and_never_opens_or_trusts_a_url() {
        assert!(validate_grok_auth_gate(&json!({"_meta":{}})).is_ok());
        let gated = validate_grok_auth_gate(&json!({
            "_meta":{"gate":{
                "message":"Subscription required",
                "label":"Upgrade",
                "url":"https://x.ai/account"
            }}
        }))
        .unwrap_err()
        .to_string();
        assert!(gated.contains("Subscription required"));
        assert!(gated.contains("https://x.ai/account"));

        let unsafe_url = validate_grok_auth_gate(&json!({
            "_meta":{"gate":{"message":"Blocked","url":"file:///tmp/secret"}}
        }))
        .unwrap_err()
        .to_string();
        assert!(unsafe_url.contains("Blocked"));
        assert!(!unsafe_url.contains("file:///"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn parses_standard_text_thought_tools_model_and_usage() {
        let mut tools = ToolState::default();
        assert_eq!(
            parse_session_update(
                &json!({"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}),
                &mut tools,
            ),
            vec![SessionEvent::TextDelta("hi".into())]
        );
        assert_eq!(
            parse_session_update(
                &json!({"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"reason"}}}),
                &mut tools,
            ),
            vec![SessionEvent::ThinkingDelta("reason".into())]
        );
        let call = parse_session_update(
            &json!({"update":{"sessionUpdate":"tool_call","toolCallId":"t1","kind":"execute","title":"Run tests","rawInput":{"command":"cargo test"}}}),
            &mut tools,
        );
        assert!(
            matches!(&call[0], SessionEvent::ToolCallCorrelated { name, call_id, .. } if name == "Bash" && call_id == "t1")
        );
        let progress = parse_session_update(
            &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"Running cargo test"}}),
            &mut tools,
        );
        assert_eq!(
            progress,
            vec![SessionEvent::ToolProgressCorrelated {
                call_id: "t1".into(),
                title: "Running cargo test".into(),
            }]
        );
        let done = parse_session_update(
            &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","rawOutput":"ok"}}),
            &mut tools,
        );
        assert_eq!(
            done,
            vec![SessionEvent::ToolResultCorrelated {
                call_id: "t1".into(),
                ok: true,
                summary: "ok".into()
            }]
        );
        assert!(parse_session_update(
            &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"late progress"}}),
            &mut tools,
        )
        .is_empty());
        assert_eq!(
            extract_session_model(
                &json!({"configOptions":[{"id":"model","currentValue":"grok-code-fast-1"}]})
            ),
            Some("grok-code-fast-1".into())
        );
        assert_eq!(
            parse_usage(&json!({
                "inputTokens":12,"outputTokens":7,"totalTokens":19,
                "cachedReadTokens":0,"reasoningTokens":0,
                "modelCalls":1,"numTurns":1
            })),
            Some(Usage {
                model_calls: 1,
                num_turns: 1,
                ..Usage::exact(12, 7)
            })
        );
        let (status, usage) = parse_prompt_result(&json!({
            "stopReason":"end_turn",
            "_meta":{
                "inputTokens":120,
                "outputTokens":70,
                "cachedReadTokens":90,
                "reasoningTokens":30,
                "usage":{"inputTokens":500,"outputTokens":300,"totalTokens":800,
                    "cachedReadTokens":400,"reasoningTokens":100,
                    "modelCalls":3,"numTurns":2}
            }
        }));
        assert_eq!(status, TurnStatus::Completed);
        assert_eq!(
            usage,
            Some(Usage {
                cached_read_tokens: 400,
                reasoning_tokens: 100,
                model_calls: 3,
                num_turns: 2,
                ..Usage::exact(500, 300)
            }),
            "whole-prompt usage must win over last-call sibling fields"
        );
        assert_eq!(
            usage_from_event(&json!({"update":{
                "sessionUpdate":"turn_completed",
                "usage":{"inputTokens":500,"outputTokens":300,"totalTokens":800,
                    "cachedReadTokens":400,"reasoningTokens":100,
                    "modelCalls":3,"numTurns":2}
            }})),
            Some(Usage {
                cached_read_tokens: 400,
                reasoning_tokens: 100,
                model_calls: 3,
                num_turns: 2,
                ..Usage::exact(500, 300)
            })
        );
    }

    #[test]
    fn standard_tool_result_retains_the_tui_log_budget_without_becoming_unbounded() {
        let exact = "x".repeat(MAX_TOOL_RESULT_CHARS);
        assert_eq!(
            tool_output_summary(&json!({"rawOutput": exact})),
            "x".repeat(MAX_TOOL_RESULT_CHARS)
        );

        let oversized = "y".repeat(MAX_TOOL_RESULT_CHARS + 100);
        let summary = tool_output_summary(&json!({"content":[{
            "type":"content",
            "content":{"type":"text","text":oversized}
        }]}));
        assert_eq!(summary.chars().count(), MAX_TOOL_RESULT_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn standard_presentation_deltas_and_tool_ids_are_bounded_before_queueing() {
        let oversized_delta = "x".repeat(MAX_STREAM_DELTA_CHARS + 100);
        let text = text_from_content(&json!({
            "content":{"type":"text","text":oversized_delta}
        }))
        .unwrap();
        assert_eq!(text.chars().count(), MAX_STREAM_DELTA_CHARS + 1);
        assert!(text.ends_with('…'));

        let mut tools = ToolState::default();
        let oversized_id = "i".repeat(MAX_TOOL_CALL_ID_CHARS + 1);
        let call = parse_tool_call(
            &json!({
                "toolCallId":oversized_id,
                "title":"Read file",
                "kind":"read",
                "rawInput":{"path":"src/lib.rs"}
            }),
            &mut tools,
        );
        assert!(matches!(call.as_slice(), [SessionEvent::ToolCall { .. }]));
        assert!(tools.known.is_empty());
        assert!(tools.provisional.is_empty());
        assert!(tools.settled.is_empty());

        let controlled = parse_tool_call(
            &json!({
                "toolCallId":"tool\nforged",
                "title":"Read file",
                "kind":"read",
                "rawInput":{"path":"src/lib.rs"}
            }),
            &mut tools,
        );
        assert!(matches!(
            controlled.as_slice(),
            [SessionEvent::ToolCall { .. }]
        ));
        assert!(tools.known.is_empty());

        let exact = "z".repeat(MAX_TOOL_CALL_ID_CHARS);
        assert_eq!(
            bounded_tool_call_id(&json!({"toolCallId":exact})),
            "z".repeat(MAX_TOOL_CALL_ID_CHARS)
        );
    }

    #[test]
    fn grok_whole_prompt_usage_preserves_quality_cost_and_u64_width() {
        let above_u32 = u64::from(u32::MAX) + 17;
        let exact = parse_usage(&json!({
            "inputTokens":above_u32,"outputTokens":9,
            "totalTokens":above_u32 + 9,"cachedReadTokens":7,
            "reasoningTokens":3,"modelCalls":4,"apiDurationMs":99,
            "costUsdTicks":1_234_567_890_i64,"numTurns":2,
            "modelUsage":{"grok-test":{"inputTokens":above_u32}}
        }))
        .expect("official whole-prompt shape");
        assert_eq!(exact.input_tokens, above_u32);
        assert_eq!(exact.total_tokens, above_u32 + 9);
        assert_eq!(exact.cached_read_tokens, 7);
        assert_eq!(exact.reasoning_tokens, 3);
        assert_eq!(exact.model_calls, 4);
        assert_eq!(exact.num_turns, 2);
        assert_eq!(exact.trusted_cost_usd_ticks(), Some(1_234_567_890));
        assert!(!exact.usage_incomplete);

        let incomplete_empty = parse_usage(&json!({
            "inputTokens":0,"outputTokens":0,"totalTokens":0,
            "cachedReadTokens":0,"reasoningTokens":0,"modelCalls":0,
            "numTurns":0,"usageIsIncomplete":true,"costUsdTicks":99
        }))
        .expect("official empty incomplete report");
        assert!(incomplete_empty.usage_incomplete);
        assert!(incomplete_empty.has_empty_lower_bound());
        assert_eq!(incomplete_empty.cost_usd_ticks, None);

        let partial = parse_usage(&json!({
            "inputTokens":10,"outputTokens":2,"totalTokens":12,
            "cachedReadTokens":4,"reasoningTokens":1,"modelCalls":2,
            "numTurns":1,"costUsdTicks":99,"costIsPartial":true
        }))
        .expect("partial cost preserves token lower bound");
        assert!(partial.cost_partial);
        assert_eq!(partial.cost_usd_ticks, None);
        assert_eq!(partial.trusted_cost_usd_ticks(), None);

        let missing_cost = parse_usage(&json!({
            "inputTokens":10,"outputTokens":2,"totalTokens":12,
            "cachedReadTokens":4,"reasoningTokens":1,"modelCalls":2,
            "numTurns":1
        }))
        .expect("missing cost is valid but unknown");
        assert_eq!(missing_cost.trusted_cost_usd_ticks(), None);
    }

    #[test]
    fn prompt_usage_present_invalid_or_absent_never_falls_back_to_last_call() {
        let sibling = json!({
            "inputTokens":120,"outputTokens":70,"totalTokens":190,
            "cachedReadTokens":90,"reasoningTokens":30,"modelCalls":1,"numTurns":1
        });
        for invalid in [
            Value::Null,
            json!({}),
            json!("bad"),
            json!({
                "inputTokens":5,"outputTokens":3,"totalTokens":999,
                "cachedReadTokens":0,"reasoningTokens":0,"modelCalls":1,"numTurns":1
            }),
        ] {
            let mut meta = sibling.clone();
            meta["usage"] = invalid;
            let (_, usage) = parse_prompt_result(&json!({
                "stopReason":"end_turn", "_meta":meta
            }));
            assert_eq!(usage, None);
        }
        let (_, absent) = parse_prompt_result(&json!({
            "stopReason":"end_turn", "_meta":sibling,
            "usage":{
                "inputTokens":5,"outputTokens":3,"totalTokens":8,
                "cachedReadTokens":0,"reasoningTokens":0,"modelCalls":1,"numTurns":1
            }
        }));
        assert_eq!(
            absent, None,
            "top-level/last-call fallback is not authoritative"
        );
        assert_eq!(parse_usage(&json!({})), None);
    }

    #[test]
    fn rpc_error_prompt_usage_is_typed_and_malformed_usage_fails_closed() {
        let error = acp_response_payload(&json!({
            "jsonrpc":"2.0","id":7,"error":{
                "code":-32000,"message":"sampling failed","data":{"promptUsage":{
                    "inputTokens":8,"outputTokens":2,"totalTokens":10,
                    "cachedReadTokens":3,"reasoningTokens":1,"modelCalls":1,
                    "numTurns":1,"usageIsIncomplete":true,"costUsdTicks":99
                }}
            }
        }))
        .unwrap_err();
        assert_eq!(error.code, Some(-32_000));
        let usage = error.prompt_usage.expect("error-path usage retained");
        assert_eq!(usage.total_tokens, 10);
        assert!(usage.usage_incomplete);
        assert_eq!(usage.cost_usd_ticks, None);

        let malformed = acp_response_payload(&json!({
            "id":8,"error":{"message":"bad","data":{"promptUsage":{}}}
        }))
        .unwrap_err();
        assert_eq!(malformed.prompt_usage, None);
    }

    #[test]
    fn kimi_question_parser_accepts_only_the_exact_ordered_source_shape() {
        let source_shape = json!({
            "toolCall":{
                "toolCallId":"ask-1",
                "title":"AskUserQuestion",
                "content":[{"type":"content","content":{"type":"text","text":"Choose delivery scope"}}]
            },
            "options":[
                {"optionId":"q0_opt_0","name":"Minimal","kind":"allow_once"},
                {"optionId":"q0_opt_1","name":"Complete","kind":"allow_once"},
                {"optionId":"q0_skip","name":"Skip","kind":"reject_once"}
            ]
        });
        let (question, pending) =
            kimi_permission_question(&source_shape).expect("pinned Kimi question shape");
        assert_eq!(question.id, "q0");
        assert_eq!(question.kind, HostQuestionKind::SingleChoice);
        assert!(!question.required);
        assert_eq!(question.options[1].value, "q0_opt_1");
        assert_eq!(pending.option_labels["q0_skip"], "Skip");

        let mut reordered = source_shape.clone();
        reordered["options"].as_array_mut().unwrap().swap(1, 2);
        assert!(kimi_permission_question(&reordered).is_none());

        let mut skipped_index = source_shape.clone();
        skipped_index["options"][1]["optionId"] = json!("q0_opt_7");
        assert!(kimi_permission_question(&skipped_index).is_none());

        let mut widened = source_shape.clone();
        widened["options"][0]["kind"] = json!("allow_always");
        assert!(kimi_permission_question(&widened).is_none());

        let mut renamed_skip = source_shape;
        renamed_skip["options"][2]["name"] = json!("Dismiss");
        assert!(kimi_permission_question(&renamed_skip).is_none());
    }

    #[test]
    fn kimi_permission_surface_never_downgrades_malformed_human_input_to_approval() {
        let malformed_question = json!({
            "toolCall":{
                "title":"AskUserQuestion",
                "content":[{"type":"content","content":{"type":"text","text":"Choose"}}]
            },
            "options":[{"optionId":"q0_opt_7","name":"Wrong index","kind":"allow_once"}]
        });
        assert_eq!(
            kimi_permission_surface(&malformed_question),
            KimiPermissionSurface::Question
        );
        assert!(kimi_permission_question(&malformed_question).is_none());

        let malformed_plan = json!({
            "toolCall":{
                "title":"ExitPlanMode",
                "content":[{"type":"content","content":{"type":"text","text":"# Plan"}}]
            },
            "options":[
                {"optionId":"plan_opt_0","name":"A","kind":"allow_always"},
                {"optionId":"plan_revise","name":"Revise","kind":"reject_once"},
                {"optionId":"plan_reject_and_exit","name":"Reject and Exit","kind":"reject_once"}
            ]
        });
        assert_eq!(
            kimi_permission_surface(&malformed_plan),
            KimiPermissionSurface::PlanReview
        );
        assert!(kimi_plan_review_permission(&malformed_plan).is_none());

        let ordinary = json!({
            "toolCall":{"title":"Bash"},
            "options":[
                {"optionId":"approve_once","name":"Approve once","kind":"allow_once"},
                {"optionId":"approve_always","name":"Approve always","kind":"allow_always"},
                {"optionId":"reject","name":"Reject","kind":"reject_once"}
            ]
        });
        assert_eq!(
            kimi_permission_surface(&ordinary),
            KimiPermissionSurface::Ordinary
        );
    }

    #[test]
    fn kimi_plan_review_parser_preserves_exact_source_options_and_rejects_drift() {
        let source_shape = json!({
            "toolCall":{
                "toolCallId":"plan-tool",
                "title":"ExitPlanMode",
                "content":[
                    {"type":"content","content":{"type":"text","text":"Plan saved to: /tmp/plan.md\n\n# Plan\nShip option B"}},
                    {"type":"content","content":{"type":"text","text":"Requesting approval to exit plan mode"}}
                ]
            },
            "options":[
                {"optionId":"plan_opt_0","name":"Option A","kind":"allow_once"},
                {"optionId":"plan_opt_1","name":"Option B","kind":"allow_once"},
                {"optionId":"plan_revise","name":"Revise","kind":"reject_once"},
                {"optionId":"plan_reject_and_exit","name":"Reject and Exit","kind":"reject_once"}
            ]
        });
        let (question, pending) =
            kimi_plan_review_permission(&source_shape).expect("pinned Kimi plan review shape");
        assert_eq!(question.id, "kimi_plan_review");
        assert_eq!(question.kind, HostQuestionKind::SingleChoice);
        assert!(question.required);
        assert!(question.prompt.contains("# Plan"));
        assert_eq!(question.options[1].value, "plan_opt_1");
        assert_eq!(question.options[1].label, "Option B");
        assert_eq!(pending.option_labels["plan_revise"], "Revise");

        let mut reordered = source_shape.clone();
        reordered["options"].as_array_mut().unwrap().swap(2, 3);
        assert!(kimi_plan_review_permission(&reordered).is_none());

        let mut widened = source_shape;
        widened["options"][0]["kind"] = json!("allow_always");
        assert!(kimi_plan_review_permission(&widened).is_none());
    }

    #[test]
    fn tool_updates_are_deduplicated_and_unknown_update_synthesizes_call() {
        let mut tools = ToolState::default();
        let first = parse_session_update(
            &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"late","kind":"read","status":"completed","rawInput":{"path":"src/lib.rs"},"rawOutput":"done"}}),
            &mut tools,
        );
        assert_eq!(first.len(), 2);
        assert!(matches!(first[0], SessionEvent::ToolCallCorrelated { .. }));
        assert!(matches!(
            first[1],
            SessionEvent::ToolResultCorrelated { .. }
        ));
        let duplicate = parse_session_update(
            &json!({"update":{"sessionUpdate":"tool_call_update","toolCallId":"late","status":"completed","rawOutput":"done"}}),
            &mut tools,
        );
        assert!(duplicate.is_empty());
    }

    #[test]
    fn kimi_lazy_tool_upgrade_emits_one_authoritative_call_with_structured_diff() {
        let mut tools = ToolState::default();
        let pending = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"kimi-edit-1",
                "title":"edit_file",
                "kind":"edit",
                "status":"pending",
                "content":[{"type":"content","content":{
                    "type":"text","text":"{\"path\":\"src/lib.rs\""
                }}]
            }}),
            &mut tools,
        );
        assert!(
            pending.is_empty(),
            "provisional arguments are not a factual tool call"
        );

        let args_delta = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"kimi-edit-1",
                "status":"in_progress",
                "content":[{"type":"content","content":{
                    "type":"text","text":"{\"path\":\"src/lib.rs\",\"text\":\"new\"}"
                }}]
            }}),
            &mut tools,
        );
        assert!(
            args_delta.is_empty(),
            "streamed arguments are not tool output"
        );

        let upgraded = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"kimi-edit-1",
                "title":"Update source",
                "kind":"edit",
                "status":"in_progress",
                "rawInput":{"path":"src/lib.rs"},
                "content":[
                    {"type":"diff","path":"src/lib.rs","oldText":"old()","newText":"new()"},
                    {"type":"content","content":{"type":"text","text":"{\"path\":\"src/lib.rs\"}"}}
                ]
            }}),
            &mut tools,
        );
        assert_eq!(upgraded.len(), 1, "upgrade creates one tool row only");
        let SessionEvent::ToolCallCorrelated {
            call_id,
            name,
            input,
        } = &upgraded[0]
        else {
            panic!("expected authoritative correlated tool call");
        };
        assert_eq!(call_id, "kimi-edit-1");
        assert_eq!(name, "Edit");
        assert_eq!(input["file_path"], "src/lib.rs");
        assert_eq!(input["old_string"], "old()");
        assert_eq!(input["new_string"], "new()");
        assert_eq!(
            umadev_runtime::ToolEdit::from_claude_tool_input(name, input),
            Some(umadev_runtime::ToolEdit {
                path: "src/lib.rs".to_string(),
                before: "old()".to_string(),
                after: "new()".to_string(),
            })
        );

        assert_eq!(
            parse_session_update(
                &json!({"update":{
                    "sessionUpdate":"tool_call_update",
                    "toolCallId":"kimi-edit-1",
                    "status":"completed",
                    "rawOutput":"updated"
                }}),
                &mut tools,
            ),
            vec![SessionEvent::ToolResultCorrelated {
                call_id: "kimi-edit-1".to_string(),
                ok: true,
                summary: "updated".to_string(),
            }]
        );
    }

    #[test]
    fn grok_bash_output_is_incremental_stateful_and_terminal_safe() {
        let mut tools = ToolState::default();
        let first = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"bash-1",
                "kind":"execute",
                "status":"in_progress",
                "rawOutput":{
                    "type":"Bash",
                    "output":[],
                    "output_delta":b"hello\x1b]52;c;SGV".to_vec(),
                    "output_for_prompt":"hello",
                    "exit_code":0
                }
            }}),
            &mut tools,
        );
        assert!(matches!(
            first.as_slice(),
            [SessionEvent::ToolCallCorrelated { call_id, .. }, SessionEvent::ToolOutputDeltaCorrelated { call_id: output_id, delta }]
                if call_id == "bash-1" && output_id == "bash-1" && delta == "hello"
        ));

        let second = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"bash-1",
                "status":"in_progress",
                "rawOutput":{
                    "type":"Bash",
                    "output":[],
                    "output_delta":b"sbG8=\x07\xe4\xb8\x96\xe7\x95\x8c".to_vec(),
                    "output_for_prompt":"hello\u{4e16}\u{754c}",
                    "exit_code":0
                }
            }}),
            &mut tools,
        );
        assert_eq!(
            second,
            vec![SessionEvent::ToolOutputDeltaCorrelated {
                call_id: "bash-1".into(),
                delta: "\u{4e16}\u{754c}".into(),
            }]
        );

        let terminal = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"bash-1",
                "status":"completed",
                "rawOutput":{
                    "type":"Bash",
                    "output":b"hello\x1b]52;c;SGVsbG8=\x07\xe4\xb8\x96\xe7\x95\x8c".to_vec(),
                    "output_for_prompt":"\x1b[31mhello\u{4e16}\u{754c}\x1b[0m",
                    "exit_code":0
                }
            }}),
            &mut tools,
        );
        assert_eq!(
            terminal,
            vec![
                SessionEvent::ToolOutputSnapshotCorrelated {
                    call_id: "bash-1".into(),
                    output: "hello\u{4e16}\u{754c}".into(),
                },
                SessionEvent::ToolResultCorrelated {
                    call_id: "bash-1".into(),
                    ok: true,
                    summary: "hello\u{4e16}\u{754c}".into(),
                },
            ]
        );
        assert!(!format!("{terminal:?}").contains("output_delta"));
        assert!(!format!("{terminal:?}").contains("\\u001b"));
    }

    #[test]
    fn grok_bash_full_snapshots_replace_output_and_malformed_bytes_fail_open() {
        let mut tools = ToolState::default();
        let update = |output: &[u8]| {
            json!({"update":{
                "sessionUpdate":"tool_call_update",
                "toolCallId":"bash-snapshot",
                "kind":"execute",
                "status":"in_progress",
                "rawOutput":{
                    "type":"Bash",
                    "output":output,
                    "output_for_prompt":"",
                    "exit_code":0
                }
            }})
        };
        let first = parse_session_update(&update(b"abc"), &mut tools);
        assert!(matches!(
            first.last(),
            Some(SessionEvent::ToolOutputSnapshotCorrelated { output, .. }) if output == "abc"
        ));
        assert_eq!(
            parse_session_update(&update(b"abcdef"), &mut tools),
            vec![SessionEvent::ToolOutputSnapshotCorrelated {
                call_id: "bash-snapshot".into(),
                output: "abcdef".into(),
            }]
        );
        let reset = json!({"update":{
            "sessionUpdate":"tool_call_update",
            "toolCallId":"bash-snapshot",
            "status":"in_progress",
            "rawOutput":{
                "type":"Bash",
                "output":[],
                "output_delta":[]
            }
        }});
        assert_eq!(
            parse_session_update(&reset, &mut tools),
            vec![SessionEvent::ToolOutputSnapshotCorrelated {
                call_id: "bash-snapshot".into(),
                output: String::new(),
            }]
        );

        let malformed = json!({"update":{
            "sessionUpdate":"tool_call_update",
            "toolCallId":"bad-bash",
            "kind":"execute",
            "status":"in_progress",
            "rawOutput":{
                "type":"Bash",
                "output_delta":[0, 256, "not-a-byte"],
                "output":[]
            }
        }});
        let events = parse_session_update(&malformed, &mut tools);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], SessionEvent::ToolCallCorrelated { .. }));
    }

    #[test]
    fn permission_options_prefer_one_shot_and_roundtrip_ids_are_typed() {
        let params = json!({"options":[
            {"optionId":"always","kind":"allow_always"},
            {"optionId":"once","kind":"allow_once"},
            {"optionId":"deny","kind":"reject_once"}
        ]});
        let record = ApprovalRecord {
            raw_id: json!(1),
            options: parse_approval_options(&params),
        };
        assert_eq!(
            record.option_for_decision(ApprovalDecision::Allow),
            Some("once".into())
        );
        assert_eq!(
            record.option_for_decision(ApprovalDecision::Deny),
            Some("deny".into())
        );
        assert_eq!(rpc_id_string(&json!(7)), "n:7");
        assert_eq!(rpc_id_string(&json!("7")), "s:7");

        let persistent_only = ApprovalRecord {
            raw_id: json!(2),
            options: parse_approval_options(&json!({"options":[
                {"optionId":"always","kind":"allow_always"}
            ]})),
        };
        assert_eq!(
            persistent_only.option_for_decision(ApprovalDecision::Allow),
            None
        );
        assert_eq!(
            persistent_only.option_for_decision(ApprovalDecision::Deny),
            None
        );
    }

    #[test]
    fn kimi_ordinary_permission_options_match_the_exact_source_order_and_meanings() {
        let source = json!({"options":[
            {"optionId":"approve_once","name":"Approve once","kind":"allow_once"},
            {"optionId":"approve_always","name":"Approve for this session","kind":"allow_always"},
            {"optionId":"reject","name":"Reject","kind":"reject_once"}
        ]});
        let options = kimi_ordinary_permission_options(&source).expect("audited Kimi options");
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].kind, HostApprovalOptionKind::AllowOnce);
        assert_eq!(options[1].kind, HostApprovalOptionKind::AllowAlways);
        assert_eq!(options[2].kind, HostApprovalOptionKind::RejectOnce);

        let mut reordered = source.clone();
        reordered["options"].as_array_mut().unwrap().swap(0, 1);
        assert!(kimi_ordinary_permission_options(&reordered).is_none());

        let mut renamed = source.clone();
        renamed["options"][1]["name"] = json!("Always");
        assert!(kimi_ordinary_permission_options(&renamed).is_none());

        let mut widened = source;
        widened["options"].as_array_mut().unwrap().push(json!({
            "optionId":"yolo","name":"Allow everything","kind":"allow_always"
        }));
        assert!(kimi_ordinary_permission_options(&widened).is_none());
    }

    fn blocking_test_questions() -> Vec<PendingQuestion> {
        vec![
            PendingQuestion {
                id: "q1".to_string(),
                prompt: "Which database?".to_string(),
                option_labels: HashMap::from([
                    ("pg".to_string(), "Postgres".to_string()),
                    ("Postgres".to_string(), "Postgres".to_string()),
                ]),
                option_previews: HashMap::from([
                    ("pg".to_string(), "CREATE TABLE users (...)".to_string()),
                    (
                        "Postgres".to_string(),
                        "CREATE TABLE users (...)".to_string(),
                    ),
                ]),
                multi_select: false,
            },
            PendingQuestion {
                id: "q2".to_string(),
                prompt: "Anything else?".to_string(),
                option_labels: HashMap::new(),
                option_previews: HashMap::new(),
                multi_select: false,
            },
        ]
    }

    fn assert_grok_accepted_outcome(questions: &[PendingQuestion]) {
        let accepted = HostUserInputOutcome::Accepted {
            answers: vec![
                HostAnswer {
                    question_id: "q1".to_string(),
                    values: vec!["pg".to_string()],
                },
                // Source-shaped notes-only answers become the visible Other
                // label; the local id must never leak onto the wire.
                HostAnswer {
                    question_id: "q2".to_string(),
                    values: Vec::new(),
                },
            ],
            annotations: vec![
                HostQuestionAnnotation {
                    question_id: "q1".to_string(),
                    preview: Some("CREATE TABLE users (...)".to_string()),
                    notes: None,
                },
                HostQuestionAnnotation {
                    question_id: "q2".to_string(),
                    preview: None,
                    notes: Some("Keep migrations reversible".to_string()),
                },
            ],
        };
        assert_eq!(
            grok_user_input_outcome_result(questions, &accepted).unwrap(),
            json!({
                "outcome":"accepted",
                "answers":{
                    "Which database?":["Postgres"],
                    "Anything else?":["Other"]
                },
                "annotations":{
                    "Which database?":{"preview":"CREATE TABLE users (...)"},
                    "Anything else?":{"notes":"Keep migrations reversible"}
                }
            })
        );
    }

    fn assert_grok_partial_and_plan_outcomes(questions: &[PendingQuestion]) {
        let partial = vec![HostAnswer {
            question_id: "q1".to_string(),
            values: vec!["pg".to_string()],
        }];
        assert_eq!(
            grok_user_input_outcome_result(
                questions,
                &HostUserInputOutcome::ChatAboutThis {
                    partial_answers: partial.clone()
                }
            )
            .unwrap(),
            json!({
                "outcome":"chat_about_this",
                "partial_answers":{"Which database?":"Postgres"}
            })
        );
        assert_eq!(
            grok_user_input_outcome_result(
                questions,
                &HostUserInputOutcome::SkipInterview {
                    partial_answers: partial
                }
            )
            .unwrap(),
            json!({
                "outcome":"skip_interview",
                "partial_answers":{"Which database?":"Postgres"}
            })
        );
        assert_eq!(
            grok_user_input_outcome_result(questions, &HostUserInputOutcome::Cancelled).unwrap(),
            json!({"outcome":"cancelled"})
        );

        for (outcome, expected) in [
            (HostPlanOutcome::Approved, json!({"outcome":"approved"})),
            (
                HostPlanOutcome::Cancelled {
                    feedback: Some("add rollback".to_string()),
                },
                json!({"outcome":"cancelled","feedback":"add rollback"}),
            ),
            (HostPlanOutcome::Abandoned, json!({"outcome":"abandoned"})),
        ] {
            assert_eq!(grok_plan_outcome_result(&outcome).unwrap(), expected);
        }
    }

    fn assert_grok_legacy_user_input(questions: &[PendingQuestion]) {
        // The old helper still models free-form generic answers for its legacy
        // callers, but it must preserve source labels and prompt keys.
        assert_eq!(
            grok_user_input_result(
                questions,
                &[
                    HostAnswer {
                        question_id: "q1".to_string(),
                        values: vec!["pg".to_string()],
                    },
                    HostAnswer {
                        question_id: "q2".to_string(),
                        values: vec!["Keep migrations reversible".to_string()],
                    },
                ],
            ),
            json!({
                "outcome":"accepted",
                "answers":{
                    "Which database?":["Postgres"],
                    "Anything else?":["Other"]
                },
                "annotations":{
                    "Which database?":{"preview":"CREATE TABLE users (...)"},
                    "Anything else?":{"notes":"Keep migrations reversible"}
                }
            })
        );
    }

    #[test]
    fn grok_blocking_interactions_use_their_published_wire_outcomes() {
        assert!(is_grok_ask_user_question_method("x.ai/ask_user_question"));
        assert!(is_grok_exit_plan_mode_method("x.ai/exit_plan_mode"));
        let questions = blocking_test_questions();
        assert_grok_accepted_outcome(&questions);
        assert_grok_partial_and_plan_outcomes(&questions);
        assert_grok_legacy_user_input(&questions);
    }

    #[test]
    fn grok_blocking_request_shapes_are_source_strict() {
        let ask = json!({
            "sessionId":"s1",
            "toolCallId":"tc1",
            "mode":"plan",
            "questions":[{
                "id":"q1",
                "question":"DB?",
                "options":[{
                    "id":"o1","label":"Redis","description":"cache","preview":"diff"
                }],
                "multiSelect":null
            }]
        });
        assert_eq!(validate_grok_ask_user_question(&ask), Ok(()));
        let mut duplicate = ask.clone();
        duplicate["questions"] = json!([
            {"question":"same","options":[],"multiSelect":false},
            {"question":"same","options":[],"multiSelect":false}
        ]);
        assert!(validate_grok_ask_user_question(&duplicate).is_err());
        let mut malformed = ask;
        malformed["mode"] = json!("agent");
        assert!(validate_grok_ask_user_question(&malformed).is_err());

        assert_eq!(
            validate_grok_exit_plan_mode(&json!({
                "sessionId":"s1","toolCallId":"tc2","planContent":null
            })),
            Ok(())
        );
        assert!(validate_grok_exit_plan_mode(&json!({
            "sessionId":"s1","toolCallId":"tc2"
        }))
        .is_err());

        assert!(!is_grok_ask_user_question_method(
            "x.ai/not_really_ask_user_question"
        ));
        assert!(!is_grok_exit_plan_mode_method(
            "x.ai/fake_plan_confirmation"
        ));
        assert!(!is_permission_expansion_method(
            "x.ai/fake_permission_expansion"
        ));
    }

    #[test]
    fn one_shot_runtime_never_bypasses_upstream_permission_boundaries() {
        let approval = HostRequest::Approval {
            action: "Bash".to_string(),
            target: "cargo test".to_string(),
            message: None,
            options: Vec::new(),
            metadata: Value::Null,
        };
        assert!(matches!(
            one_shot_host_response(&approval, BasePermissionProfile::Auto),
            HostResponse::Approval {
                decision: ApprovalDecision::Deny,
                ..
            }
        ));
        assert!(matches!(
            one_shot_host_response(&approval, BasePermissionProfile::Guarded),
            HostResponse::Approval {
                decision: ApprovalDecision::Deny,
                ..
            }
        ));

        let requested = HostPermission {
            kind: "network".to_string(),
            target: Some("127.0.0.1:3000".to_string()),
            metadata: Value::Null,
        };
        let expansion = HostRequest::PermissionExpansion {
            permissions: vec![requested.clone()],
            reason: Some("run the development server".to_string()),
            metadata: Value::Null,
        };
        assert_eq!(
            one_shot_host_response(&expansion, BasePermissionProfile::Auto),
            HostResponse::PermissionExpansion {
                decision: ApprovalDecision::Deny,
                granted: Vec::new(),
                message: Some("one-shot ACP runtime has no interactive host surface".to_string()),
            }
        );
    }

    #[test]
    fn cancelled_host_responses_preserve_protocol_safe_outcomes() {
        let approval = PendingHostRequest::Approval(ApprovalRecord {
            raw_id: json!(9),
            options: parse_approval_options(&json!({"options":[
                {"optionId":"reject-once","name":"Reject","kind":"reject_once"}
            ]})),
        });
        assert_eq!(
            cancelled_host_response_frame(approval, Some("cancelled".to_string())),
            json!({
                "jsonrpc":"2.0", "id":9,
                "result":{"outcome":{"outcome":"cancelled"}}
            })
        );
        assert_eq!(
            cancelled_host_response_frame(
                PendingHostRequest::McpElicitation {
                    raw_id: json!("m1")
                },
                None,
            ),
            json!({"jsonrpc":"2.0", "id":"m1", "result":{"action":"cancel"}})
        );
        assert_eq!(
            cancelled_host_response_frame(
                PendingHostRequest::UserInput {
                    raw_id: json!("ask-1"),
                    questions: Vec::new(),
                    flavor: UserInputFlavor::GrokAskUserQuestion,
                },
                Some("esc".to_string()),
            ),
            json!({"jsonrpc":"2.0", "id":"ask-1", "result":{"outcome":"cancelled"}})
        );
        assert_eq!(
            cancelled_host_response_frame(
                PendingHostRequest::UserInput {
                    raw_id: json!("kimi-plan-1"),
                    questions: Vec::new(),
                    flavor: UserInputFlavor::KimiPlanReview,
                },
                Some("headless".to_string()),
            ),
            json!({
                "jsonrpc":"2.0", "id":"kimi-plan-1",
                "result":{"outcome":{"outcome":"cancelled"}}
            })
        );
        assert_eq!(
            cancelled_host_response_frame(
                PendingHostRequest::PlanConfirmation {
                    raw_id: json!("plan-1"),
                    flavor: PlanConfirmationFlavor::GrokExitPlanMode,
                },
                Some("revise".to_string()),
            ),
            json!({
                "jsonrpc":"2.0", "id":"plan-1",
                "result":{"outcome":"cancelled","feedback":"revise"}
            })
        );
    }

    #[test]
    fn permission_followup_message_is_bounded_redacted_response_metadata() {
        let message = format!(
            "Authorization: Bearer must-not-leak {}",
            "x".repeat(MAX_PERMISSION_FOLLOWUP_CHARS + 100)
        );
        let frame = permission_response_frame(&json!(17), Some("reject-once"), Some(&message));
        assert_eq!(
            frame.pointer("/result/outcome/optionId"),
            Some(&json!("reject-once"))
        );
        assert!(frame.pointer("/result/outcome/_meta").is_none());
        let followup = frame
            .pointer("/result/_meta/followup_message")
            .and_then(Value::as_str)
            .unwrap();
        assert!(!followup.contains("must-not-leak"));
        assert!(followup.chars().count() <= MAX_PERMISSION_FOLLOWUP_CHARS);
    }

    #[test]
    fn secret_notifications_are_dropped_recursively_and_values_redacted() {
        let grok = json!({
            "method":"_x.ai/mcp/servers_updated",
            "params":{"servers":[{"env":[
                {"name":"VENDOR_CREDENTIAL","value":"opaque-value"}
            ]}]}
        });
        assert!(suppress_notification("_x.ai/mcp/servers_updated", &grok));
        assert!(contains_sensitive_key(&grok));
        let nested = json!({"method":"vendor/update","params":{"a":{"apiKey":"secret"}}});
        assert!(suppress_notification("vendor/update", &nested));
        let sanitized = sanitize_value(json!({
            "apiKey":"abc",
            "nested":{"password":"p"},
            "env":[{"name":"CUSTOM_KEY","value":"unstructured-secret"}],
            "headers":{"x-custom-auth":"another-unstructured-secret"},
            "ok":"visible"
        }));
        assert_eq!(sanitized["apiKey"], "[redacted]");
        assert_eq!(sanitized["nested"]["password"], "[redacted]");
        assert_eq!(sanitized["env"], "[redacted]");
        assert_eq!(sanitized["headers"], "[redacted]");
        assert_eq!(sanitized["ok"], "visible");

        let mut tools = ToolState::default();
        let text = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"Authorization: Bearer assistant-secret"}
            }}),
            &mut tools,
        );
        assert!(matches!(
            text.as_slice(),
            [SessionEvent::TextDelta(delta)] if !delta.contains("assistant-secret")
        ));

        let tool = parse_session_update(
            &json!({"update":{
                "sessionUpdate":"tool_call",
                "toolCallId":"safe-correlation-id",
                "title":"Authorization: Bearer tool-title-secret",
                "rawInput":{}
            }}),
            &mut tools,
        );
        assert!(matches!(
            tool.as_slice(),
            [SessionEvent::ToolCallCorrelated { call_id, name, .. }]
                if call_id == "safe-correlation-id" && !name.contains("tool-title-secret")
        ));
    }

    #[test]
    fn unknown_private_updates_fail_open_and_ask_extensions_are_visible() {
        let mut tools = ToolState::default();
        assert!(parse_session_update(
            &json!({"update":{"sessionUpdate":"future_vendor_shape","opaque":true}}),
            &mut tools,
        )
        .is_empty());
        assert!(is_standard_ask_question_method("session/ask_question"));
    }

    #[tokio::test]
    async fn bounded_reader_handles_crlf_and_discards_oversized_line() {
        let mut reader = Cursor::new(b"{\"ok\":true}\r\nnext\n".to_vec());
        let Some(FrameRead::Line(first)) = read_bounded_frame(&mut reader).await.unwrap() else {
            panic!("first line");
        };
        assert_eq!(first, "{\"ok\":true}");
        let Some(FrameRead::Line(second)) = read_bounded_frame(&mut reader).await.unwrap() else {
            panic!("second line");
        };
        assert_eq!(second, "next");

        let mut huge = vec![b'x'; MAX_FRAME_BYTES + 1];
        huge.extend_from_slice(b"\nok\n");
        let mut reader = Cursor::new(huge);
        assert!(matches!(
            read_bounded_frame(&mut reader).await.unwrap(),
            Some(FrameRead::Oversized)
        ));
        let Some(FrameRead::Line(next)) = read_bounded_frame(&mut reader).await.unwrap() else {
            panic!("line following oversized frame");
        };
        assert_eq!(next, "ok");
    }

    #[test]
    fn fake_acp_no_recovery_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_acp_no_recovery_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let response = match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"protocolVersion":1,"agentCapabilities":{}}
                })),
                Some("session/new") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"sessionId":"fresh-without-recovery"}
                })),
                _ => None,
            };
            if let Some(response) = response {
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
            }
        }
    }

    #[test]
    fn fake_acp_resume_preferred_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_acp_resume_preferred_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let response = match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"protocolVersion":1,"agentCapabilities":{
                        "loadSession":true,"sessionCapabilities":{"resume":{}}
                    }}
                })),
                Some("session/resume") => Some(json!({
                    "jsonrpc":"2.0", "id":frame["id"], "result":{}
                })),
                Some("session/load") => panic!("session/resume must be preferred"),
                _ => None,
            };
            if let Some(response) = response {
                writeln!(stdout, "{response}").unwrap();
                stdout.flush().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn fresh_fixture_starts_without_optional_recovery_capabilities() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_no_recovery_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        assert_eq!(session.session_id(), Some("fresh-without-recovery"));
        assert_eq!(session.capabilities().resume, ResumeCapability::Unsupported);
        session.end().await.unwrap();
    }

    #[tokio::test]
    async fn timed_out_optional_request_releases_its_pending_slot() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_no_recovery_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        let result = session
            .request_with_timeout(
                "vendor/no_response",
                Value::Null,
                Duration::from_millis(25),
                "fixture timeout",
            )
            .await;
        assert!(matches!(
            result,
            Err(SessionError::Send(reason)) if reason == "fixture timeout"
        ));
        assert!(session.pending.lock().await.is_empty());
        session.end().await.unwrap();
    }

    #[tokio::test]
    async fn resume_fixture_prefers_session_resume_over_load() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_resume_preferred_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            Some("resume-me"),
        )
        .await
        .unwrap();
        assert_eq!(session.session_id(), Some("resume-me"));
        assert_eq!(session.capabilities().resume, ResumeCapability::AcpResume);
        session.end().await.unwrap();
    }

    #[test]
    fn fake_grok_long_lived_background_process_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args()
                .any(|arg| arg.ends_with("fake_grok_long_lived_background_process_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{},
                            "_meta":{
                                "grokShell":true,
                                "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                            }
                        }
                    })
                )
                .unwrap(),
                Some("session/new") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"sessionId":"background-session"}
                    })
                )
                .unwrap(),
                Some("session/set_mode") => writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}})
                )
                .unwrap(),
                Some("session/prompt") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"_x.ai/task_backgrounded",
                            "params":grok_task_backgrounded(
                                "background-session",
                                "bg-server-start",
                                "dev-server",
                                "tool-server"
                            )
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{"stopReason":"end_turn"}
                        })
                    )
                    .unwrap();
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn long_lived_background_process_does_not_block_turn_done_or_redrive() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_long_lived_background_process_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        session
            .send_turn("start the development server".to_string())
            .await
            .unwrap();
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::BackgroundProcess(
                BackgroundProcessSignal::Started {
                    process: BackgroundProcessInfo { ref task_id, .. }
                }
            )) if task_id == "dev-server"
        ));
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            })
        ));
        assert!(session
            .background_processes
            .lock()
            .await
            .live
            .contains_key("dev-server"));
        assert!(session.child.lock().unwrap().try_wait().unwrap().is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(50), session.next_event())
                .await
                .is_err()
        );
        session.end().await.unwrap();
    }

    #[test]
    fn fake_grok_auto_permission_boundary_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args()
                .any(|arg| arg.ends_with("fake_grok_auto_permission_boundary_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let mut prompt_id = None;
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{},
                            "_meta":{
                                "grokShell":true,
                                "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                            }
                        }
                    })
                )
                .unwrap(),
                Some("session/new") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"sessionId":"auto-permission-session"}
                    })
                )
                .unwrap(),
                Some("session/set_mode") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"], "result":{}
                    })
                )
                .unwrap(),
                Some("session/prompt") => {
                    prompt_id = frame.get("id").cloned();
                    writeln!(stdout, "{}", json!({
                        "jsonrpc":"2.0", "id":"auto-boundary",
                        "method":"session/request_permission",
                        "params":{
                            "sessionId":"auto-permission-session",
                            "toolCall":{
                                "toolCallId":"dangerous-shell",
                                "kind":"execute",
                                "title":"Managed policy requires confirmation",
                                "rawInput":{"command":"deploy production"}
                            },
                            "options":[
                                {"optionId":"allow-once","name":"Allow once","kind":"allow_once"},
                                {"optionId":"allow-always","name":"Always","kind":"allow_always"},
                                {"optionId":"reject-once","name":"Reject","kind":"reject_once"}
                            ]
                        }
                    }))
                    .unwrap();
                }
                None if frame.get("id") == Some(&json!("auto-boundary")) => {
                    assert_eq!(
                        frame.pointer("/result/outcome/optionId"),
                        Some(&json!("allow-always"))
                    );
                    assert_eq!(
                        frame.pointer("/result/outcome/outcome"),
                        Some(&json!("selected"))
                    );
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                            "result":{"stopReason":"end_turn"}
                        })
                    )
                    .unwrap();
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn auto_permission_boundary_waits_for_explicit_host_selection() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_auto_permission_boundary_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Auto,
            None,
        )
        .await
        .unwrap();
        session.send_turn("deploy".to_string()).await.unwrap();
        let Some(SessionEvent::HostRequest {
            req_id,
            request: HostRequest::Approval {
                options, metadata, ..
            },
        }) = session.next_event().await
        else {
            panic!("Auto must surface the upstream permission boundary");
        };
        assert_eq!(metadata["requestedProfile"], "auto");
        assert_eq!(metadata["upstreamPermissionBoundary"], true);
        assert!(options.iter().any(|option| option.id == "allow-always"));
        assert!(session.approvals.lock().await.contains_key(&req_id));
        session
            .respond_host(
                &req_id,
                HostResponse::Approval {
                    decision: ApprovalDecision::Allow,
                    selected_option_id: Some("allow-always".to_string()),
                    message: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            })
        ));
        session.end().await.unwrap();
    }

    fn emit_cancel_fixture(stdout: &mut std::io::Stdout, value: impl Into<Value>) {
        let value = value.into();
        writeln!(stdout, "{value}").unwrap();
    }

    #[derive(Default)]
    struct CancelPermissionFixtureState {
        prompt_id: Option<Value>,
        cancelled: HashSet<String>,
        saw_cancel: bool,
        finished: bool,
    }

    impl CancelPermissionFixtureState {
        fn handle_frame(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => emit_cancel_fixture(
                    stdout,
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"protocolVersion":1,"agentCapabilities":{},
                            "_meta":{"grokShell":true,
                                "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION}}
                    }),
                ),
                Some("session/new") => emit_cancel_fixture(
                    stdout,
                    json!({"jsonrpc":"2.0", "id":frame["id"],
                        "result":{"sessionId":"cancel-permission-session"}}),
                ),
                Some("session/set_mode") => emit_cancel_fixture(
                    stdout,
                    json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}),
                ),
                Some("session/prompt") => self.open_permission_requests(frame, stdout),
                Some("session/cancel") => self.saw_cancel = true,
                None if is_cancel_permission_response(frame) => self.accept_cancelled(frame),
                _ => {}
            }
            self.finish_if_settled(stdout);
        }

        fn open_permission_requests(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
            self.prompt_id = frame.get("id").cloned();
            for id in ["cancel-p1", "cancel-p2"] {
                emit_cancel_fixture(
                    stdout,
                    json!({
                        "jsonrpc":"2.0", "id":id,
                        "method":"session/request_permission",
                        "params":{"sessionId":"cancel-permission-session",
                            "toolCall":{"toolCallId":format!("tool-{id}"),"kind":"execute",
                                "rawInput":{"command":"sleep 1"}},
                            "options":[{"optionId":"reject-once","name":"Reject",
                                "kind":"reject_once"}]}
                    }),
                );
            }
        }

        fn accept_cancelled(&mut self, frame: &Value) {
            let id = frame["id"].as_str().unwrap().to_string();
            assert!(
                self.cancelled.insert(id),
                "pending request answered more than once"
            );
            assert_eq!(
                frame.pointer("/result/outcome/outcome"),
                Some(&json!("cancelled"))
            );
            assert!(frame.pointer("/result/outcome/optionId").is_none());
        }

        fn finish_if_settled(&mut self, stdout: &mut std::io::Stdout) {
            if !self.saw_cancel || self.cancelled.len() != 2 || self.finished {
                return;
            }
            emit_cancel_fixture(
                stdout,
                json!({
                    "jsonrpc":"2.0", "method":"session/update",
                    "params":{"sessionId":"cancel-permission-session","update":{
                        "sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"cancelled-two-exact"}
                    }}
                }),
            );
            emit_cancel_fixture(
                stdout,
                json!({
                    "jsonrpc":"2.0", "id":self.prompt_id.take().unwrap(),
                    "result":{"stopReason":"cancelled"}
                }),
            );
            self.finished = true;
            self.saw_cancel = false;
        }
    }

    fn is_cancel_permission_response(frame: &Value) -> bool {
        matches!(
            frame.get("id").and_then(Value::as_str),
            Some("cancel-p1" | "cancel-p2")
        )
    }

    #[test]
    fn fake_grok_cancel_pending_permissions_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args()
                .any(|arg| arg.ends_with("fake_grok_cancel_pending_permissions_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        let mut state = CancelPermissionFixtureState::default();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            state.handle_frame(&frame, &mut stdout);
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn session_cancel_answers_each_pending_permission_once_as_cancelled() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_cancel_pending_permissions_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        session
            .send_turn("request twice".to_string())
            .await
            .unwrap();
        let mut request_ids = Vec::new();
        while request_ids.len() < 2 {
            let event = tokio::time::timeout(Duration::from_secs(2), session.next_event())
                .await
                .unwrap()
                .unwrap();
            if let SessionEvent::HostRequest {
                req_id,
                request: HostRequest::Approval { .. },
            } = event
            {
                request_ids.push(req_id);
            }
        }
        session.interrupt().await.unwrap();
        assert!(session.approvals.lock().await.is_empty());
        for req_id in &request_ids {
            session
                .respond_host(
                    req_id,
                    HostResponse::Cancelled {
                        reason: Some("late duplicate".to_string()),
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TextDelta("cancelled-two-exact".to_string()))
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Interrupted,
                ..
            })
        ));
        session.end().await.unwrap();
    }

    fn emit_large_replay_fixture(stdout: &mut std::io::Stdout, value: impl Into<Value>) {
        let value = value.into();
        writeln!(stdout, "{value}").unwrap();
    }

    fn emit_historical_replay_text(stdout: &mut std::io::Stdout) {
        for index in 0..600 {
            emit_large_replay_fixture(
                stdout,
                json!({
                    "jsonrpc":"2.0", "method":"session/update",
                    "params":{"sessionId":"replay-session","update":{
                        "sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":format!("old-{index}")}
                    }}
                }),
            );
        }
    }

    fn emit_historical_replay_subagents(stdout: &mut std::io::Stdout) {
        for index in 0..300 {
            emit_large_replay_fixture(
                stdout,
                json!({
                    "jsonrpc":"2.0", "method":"_x.ai/session/update",
                    "params":{"sessionId":"replay-session",
                        "_meta":{"eventId":format!("spawn-{index}")},
                        "update":{"sessionUpdate":"subagent_spawned",
                            "subagent_id":format!("old-agent-{index}"),
                            "parent_session_id":"replay-session",
                            "child_session_id":format!("old-child-{index}"),
                            "subagent_type":"general-purpose",
                            "description":"historical replay"}}
                }),
            );
            emit_large_replay_fixture(
                stdout,
                json!({
                    "jsonrpc":"2.0", "method":"_x.ai/session/update",
                    "params":{"sessionId":"replay-session",
                        "_meta":{"eventId":format!("finish-{index}")},
                        "update":{"sessionUpdate":"subagent_finished",
                            "subagent_id":format!("old-agent-{index}"),
                            "child_session_id":format!("old-child-{index}"),
                            "status":"completed","tool_calls":0,"turns":0,"duration_ms":1}}
                }),
            );
        }
    }

    fn emit_live_replay_subagent(stdout: &mut std::io::Stdout) {
        emit_large_replay_fixture(
            stdout,
            json!({
                "jsonrpc":"2.0", "method":"_x.ai/session/update",
                "params":{"sessionId":"replay-session","_meta":{"eventId":"spawn-live"},
                    "update":{"sessionUpdate":"subagent_spawned","subagent_id":"agent-live",
                        "parent_session_id":"replay-session","child_session_id":"child-live",
                        "subagent_type":"general-purpose","description":"still running"}}
            }),
        );
    }

    fn emit_replay_background_updates(stdout: &mut std::io::Stdout) {
        let mut updates = vec![
            grok_task_backgrounded("replay-session", "bg-live", "task-live", "call-live"),
            grok_task_backgrounded(
                "replay-session",
                "bg-finished",
                "task-finished",
                "call-finished",
            ),
            grok_task_completed("replay-session", "done-finished", "task-finished", "bash"),
            grok_task_completed(
                "replay-session",
                "done-before-start",
                "task-late",
                "monitor",
            ),
            grok_task_backgrounded(
                "replay-session",
                "late-after-done",
                "task-late",
                "call-late",
            ),
            grok_task_backgrounded(
                "replay-session",
                "bg-live",
                "task-duplicate-event",
                "call-duplicate-event",
            ),
        ];
        for params in &mut updates {
            params["_meta"]["isReplay"] = json!(true);
            emit_large_replay_fixture(
                stdout,
                json!({"jsonrpc":"2.0", "method":"_x.ai/session/update", "params":params}),
            );
        }
    }

    fn emit_replay_state_updates(stdout: &mut std::io::Stdout, frame: &Value) {
        for value in [
            json!({
                "jsonrpc":"2.0", "method":"session/update",
                "params":{"sessionId":"replay-session","update":{
                    "sessionUpdate":"current_mode_update","currentModeId":"plan"
                }}
            }),
            json!({
                "jsonrpc":"2.0", "method":"session/update",
                "params":{"sessionId":"replay-session","update":{
                    "sessionUpdate":"available_commands_update",
                    "availableCommands":[{"name":"compact",
                        "description":"Compact context","input":null}],
                    "_meta":{"tools":["read_file"]}
                }}
            }),
            json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}),
        ] {
            emit_large_replay_fixture(stdout, value);
        }
    }

    fn emit_live_subagent_list(stdout: &mut std::io::Stdout, frame: &Value) {
        let parent = frame["params"]["sessionId"].as_str().unwrap();
        let subagents = if parent == "replay-session" {
            vec![json!({
                "subagentId":"agent-live","parentSessionId":"replay-session",
                "childSessionId":"child-live","subagentType":"general-purpose",
                "description":"still running","startedAtEpochMs":1,"durationMs":2,
                "turnCount":0,"toolCallCount":0,"tokensUsed":0,
                "contextWindowTokens":100_000,"contextUsagePct":0,"toolsUsed":[],
                "errorCount":0
            })]
        } else {
            Vec::new()
        };
        emit_large_replay_fixture(
            stdout,
            json!({"jsonrpc":"2.0", "id":frame["id"], "result":{"subagents":subagents}}),
        );
    }

    fn emit_live_after_load(stdout: &mut std::io::Stdout, frame: &Value) {
        emit_large_replay_fixture(
            stdout,
            json!({
                "jsonrpc":"2.0", "method":"session/update",
                "params":{"sessionId":"replay-session","update":{
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"live-after-load"}
                }}
            }),
        );
        emit_large_replay_fixture(
            stdout,
            json!({"jsonrpc":"2.0", "id":frame["id"], "result":{"stopReason":"end_turn"}}),
        );
    }

    #[test]
    fn fake_grok_large_load_replay_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_grok_large_load_replay_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{
                                "protocolVersion":1,
                                "agentCapabilities":{"loadSession":true},
                                "_meta":{
                                    "grokShell":true,
                                    "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                                }
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/load") => {
                    emit_historical_replay_text(&mut stdout);
                    emit_historical_replay_subagents(&mut stdout);
                    emit_live_replay_subagent(&mut stdout);
                    emit_replay_background_updates(&mut stdout);
                    emit_replay_state_updates(&mut stdout, &frame);
                }
                Some("_x.ai/subagent/list_running") => {
                    emit_live_subagent_list(&mut stdout, &frame);
                }
                Some("session/set_mode") => {
                    emit_large_replay_fixture(
                        &mut stdout,
                        json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}),
                    );
                }
                Some("session/prompt") => {
                    emit_live_after_load(&mut stdout, &frame);
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn large_grok_load_replay_does_not_deadlock_or_leak_old_transcript() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_large_load_replay_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = tokio::time::timeout(
            Duration::from_secs(5),
            AcpSession::start_with_program_args(
                AcpVendor::Grok,
                executable.to_str().unwrap(),
                args,
                workspace.path(),
                "",
                BasePermissionProfile::Guarded,
                Some("replay-session"),
            ),
        )
        .await
        .expect("large source-shaped replay must not deadlock")
        .unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundTask(BackgroundTaskSignal::Live {
                agent_ids: vec!["agent-live".to_string()]
            }))
        );
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(SessionStateUpdate::ModeChanged {
                mode: SessionMode::Plan,
            }))
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(
                SessionStateUpdate::CommandCatalogReplaced { commands, tools }
            )) if commands.len() == 1 && commands[0].name == "compact" && tools == ["read_file"]
        ));
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::BackgroundProcess(
                BackgroundProcessSignal::Live {
                    processes: vec![BackgroundProcessInfo {
                        task_id: "task-live".to_string(),
                        tool_call_id: "call-live".to_string(),
                        kind: BackgroundProcessKind::Bash,
                        description: Some("Development server".to_string()),
                    }]
                }
            ))
        );
        session.send_turn("continue".to_string()).await.unwrap();
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::TextDelta("live-after-load".to_string()))
        );
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            })
        ));
        session.end().await.unwrap();
    }

    #[tokio::test]
    async fn audited_cached_auth_skips_authenticate_and_controls_typed_session_state() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_grok_cached_auth_state_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        assert_eq!(session.session_id(), Some("state-session"));
        assert!(session.capabilities().supports(SessionCapability::SetModel));
        assert!(session.capabilities().supports(SessionCapability::SetMode));

        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(
                SessionStateUpdate::ModelCatalogReplaced { ref current_model_id, .. }
            )) if current_model_id == "model-a"
        ));
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(
                SessionStateUpdate::CommandCatalogReplaced { .. }
            ))
        ));
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(
                SessionStateUpdate::ModelCatalogReplaced { ref current_model_id, .. }
            )) if current_model_id == "model-a"
        ));
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::SessionModel("model-a".to_string()))
        );

        session
            .set_model("model-b".to_string(), Some(SessionReasoningEffort::High))
            .await
            .unwrap();
        assert_eq!(session.model, "model-b");
        session.set_mode(SessionMode::Ask).await.unwrap();
        session.end().await.unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fake_grok_cached_auth_state_child() {
        let invoked_as_fixture = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_grok_cached_auth_state_child"));
        if !invoked_as_fixture {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    writeln!(stdout, "{}", json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{"loadSession":true},
                            "authMethods":[{"id":"cached_token"},{"id":"grok.com"}],
                            "_meta":{
                                "grokShell":true,
                                "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION,
                                "defaultAuthMethodId":"cached_token",
                                "modelState":state_fixture_model_catalog(),
                                "availableCommands":[{
                                    "name":"compact", "description":"Compact context", "input":null
                                }]
                            }
                        }
                    }))
                    .unwrap();
                }
                Some("authenticate") => {
                    panic!("audited cached_token must not be authenticated twice");
                }
                Some("session/new") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{
                                "sessionId":"state-session",
                                "models":state_fixture_model_catalog(),
                                "_meta":{"x.ai/sessionConfig":{"options":[]}}
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/set_mode") => {
                    let mode = frame.pointer("/params/modeId").and_then(Value::as_str);
                    assert!(matches!(mode, Some("default" | "ask")));
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{}
                        })
                    )
                    .unwrap();
                }
                Some("session/set_model") => {
                    assert_eq!(
                        frame.pointer("/params/modelId").and_then(Value::as_str),
                        Some("model-b")
                    );
                    assert_eq!(
                        frame
                            .pointer("/params/_meta/reasoningEffort")
                            .and_then(Value::as_str),
                        Some("high")
                    );
                    // This mirrors the pinned source's currently unreliable
                    // response body. The client must ignore it after RPC success.
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"],
                            "result":{"_meta":{"model":{"Ok":"routing-model"}}}
                        })
                    )
                    .unwrap();
                }
                Some("_x.ai/session/close") => {
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{}
                        })
                    )
                    .unwrap();
                    stdout.flush().unwrap();
                    break;
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    fn state_fixture_model_catalog() -> Value {
        json!({
            "currentModelId":"model-a",
            "availableModels":[
                {"modelId":"model-a","name":"Model A"},
                {"modelId":"model-b","name":"Model B","_meta":{
                    "supportsReasoningEffort":true,
                    "reasoningEfforts":["low","high"]
                }}
            ]
        })
    }

    fn kimi_fixture_config_options(model: &str, mode: &str) -> Value {
        kimi_fixture_config_options_with_thinking(model, mode, "on")
    }

    fn kimi_fixture_config_options_with_thinking(model: &str, mode: &str, thinking: &str) -> Value {
        json!([
            {
                "type":"select", "id":"model", "name":"Model",
                "category":"model", "currentValue":model,
                "options":[
                    {"value":"model-a","name":"Model A"},
                    {"value":"model-b","name":"Model B","description":"Second model"}
                ]
            },
            {
                "type":"select", "id":"thinking", "name":"Thinking",
                "category":"thought_level", "currentValue":thinking,
                "options":[{"value":"off","name":"Off"},{"value":"on","name":"On"}]
            },
            {
                "type":"select", "id":"mode", "name":"Mode",
                "category":"mode", "currentValue":mode,
                "options":[
                    {"value":"default","name":"Default"},
                    {"value":"plan","name":"Plan"},
                    {"value":"auto","name":"Auto"},
                    {"value":"yolo","name":"YOLO"}
                ]
            }
        ])
    }

    #[test]
    fn fake_kimi_auth_required_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_kimi_auth_required_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        for line in stdin.lock().lines().map_while(Result::ok) {
            let frame: Value = serde_json::from_str(&line).unwrap();
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"], "result":{
                            "protocolVersion":1,
                            "agentInfo":{
                                "name":"Kimi Code CLI",
                                "version":crate::kimi_contract::KIMI_CODE_SOURCE_VERSION
                            },
                            "agentCapabilities":{"loadSession":true},
                            "authMethods":[{"id":"login","type":"terminal","args":["--login"]}]
                        }
                    })
                )
                .unwrap(),
                Some("authenticate") => writeln!(
                    stdout,
                    "{}",
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "error":{"code":-32_000,"message":"Authentication required"}
                    })
                )
                .unwrap(),
                Some(other) => panic!("Kimi unauthenticated open sent unexpected method {other}"),
                None => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn kimi_auth_required_is_actionable_without_automatic_browser_or_session_open() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_auth_required_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let error = match AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        {
            Ok(mut session) => {
                session.end().await.unwrap();
                panic!("unauthenticated Kimi fixture unexpectedly opened")
            }
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("kimi login"));
        assert!(message.contains("never open the login browser automatically"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fake_kimi_source_contract_child() {
        let invoked = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_kimi_source_contract_child"));
        if !invoked {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        let mut prompt_id = None;
        for line in stdin.lock().lines().map_while(Result::ok) {
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    assert!(frame["params"].get("_meta").is_none());
                    assert!(frame["params"]["clientCapabilities"].get("_meta").is_none());
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{
                                "protocolVersion":1,
                                "agentInfo":{
                                    "name":"Kimi Code CLI",
                                    "version":crate::kimi_contract::KIMI_CODE_SOURCE_VERSION
                                },
                                "agentCapabilities":{
                                    "loadSession":true,
                                    "promptCapabilities":{"image":true,"embeddedContext":true},
                                    "sessionCapabilities":{"list":{},"resume":{}}
                                },
                                "authMethods":[{
                                    "id":"login","type":"terminal","name":"Login",
                                    "args":["--login"]
                                }]
                            }
                        })
                    )
                    .unwrap();
                }
                Some("authenticate") => {
                    assert_eq!(frame["params"]["methodId"], "login");
                    std::fs::write("kimi-auth-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{}
                        })
                    )
                    .unwrap();
                }
                Some("session/new") => {
                    assert!(frame["params"].get("_meta").is_none());
                    std::fs::write("kimi-new-standard-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{
                                "sessionId":"kimi-fixture-session",
                                "configOptions":kimi_fixture_config_options("model-a", "default")
                            }
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update", "params":{
                                "sessionId":"kimi-fixture-session", "update":{
                                    "sessionUpdate":"available_commands_update",
                                    "availableCommands":[
                                        {"name":"compact","description":"Compact the conversation context",
                                         "input":{"hint":"<optional custom summarization instructions>"}},
                                        {"name":"status","description":"Show current session status"},
                                        {"name":"usage","description":"Show session token usage"},
                                        {"name":"mcp","description":"Show MCP server status"},
                                        {"name":"tasks","description":"List background tasks"},
                                        {"name":"help","description":"Show available ACP commands"},
                                        {"name":"skill:review","description":"Review the current patch"}
                                    ]
                                }
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/set_config_option") => {
                    let config_id = frame["params"]["configId"].as_str().unwrap();
                    let value = frame["params"]["value"].as_str().unwrap();
                    let (model, mode, thinking, marker) = match config_id {
                        "model" => (value, "default", "on", "kimi-model-observed"),
                        "mode" => ("model-b", value, "on", "kimi-mode-observed"),
                        "thinking" => ("model-b", "default", value, "kimi-thinking-observed"),
                        other => panic!("unexpected Kimi config option {other}"),
                    };
                    std::fs::write(marker, b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{
                                "configOptions":kimi_fixture_config_options_with_thinking(
                                    model, mode, thinking
                                )
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/prompt") => {
                    assert!(frame["params"].get("_meta").is_none());
                    let text = frame["params"]["prompt"][0]["text"].as_str().unwrap();
                    assert!(matches!(
                        text,
                        "hello kimi"
                            | "cancel me"
                            | "ask me"
                            | "review plan"
                            | "/skill:review --focus src  "
                    ));
                    prompt_id = frame.get("id").cloned();
                    if text == "/skill:review --focus src  " {
                        std::fs::write("kimi-native-skill-observed", b"ok").unwrap();
                        writeln!(
                            stdout,
                            "{}",
                            json!({
                                "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                                "result":{"stopReason":"end_turn"}
                            })
                        )
                        .unwrap();
                        continue;
                    }
                    if text == "cancel me" {
                        std::fs::write("kimi-cancellable-turn-observed", b"ok").unwrap();
                        stdout.flush().unwrap();
                        continue;
                    }
                    if text == "ask me" {
                        writeln!(stdout, "{}", json!({
                            "jsonrpc":"2.0", "id":"kimi-question",
                            "method":"session/request_permission", "params":{
                                "sessionId":"kimi-fixture-session",
                                "toolCall":{
                                    "toolCallId":"ask-user",
                                    "title":"AskUserQuestion",
                                    "content":[{"type":"content","content":{
                                        "type":"text","text":"Which implementation?"
                                    }}]
                                },
                                "options":[
                                    {"optionId":"q0_opt_0","name":"Minimal","kind":"allow_once"},
                                    {"optionId":"q0_opt_1","name":"Complete","kind":"allow_once"},
                                    {"optionId":"q0_skip","name":"Skip","kind":"reject_once"}
                                ]
                            }
                        })).unwrap();
                        stdout.flush().unwrap();
                        continue;
                    }
                    if text == "review plan" {
                        writeln!(stdout, "{}", json!({
                            "jsonrpc":"2.0", "id":"kimi-plan-review",
                            "method":"session/request_permission", "params":{
                                "sessionId":"kimi-fixture-session",
                                "toolCall":{
                                    "toolCallId":"exit-plan-mode",
                                    "title":"ExitPlanMode",
                                    "content":[
                                        {"type":"content","content":{
                                            "type":"text","text":"Plan saved to: /tmp/plan.md\n\n# Delivery plan\nChoose the complete implementation."
                                        }},
                                        {"type":"content","content":{
                                            "type":"text","text":"Requesting approval to exit plan mode"
                                        }}
                                    ]
                                },
                                "options":[
                                    {"optionId":"plan_opt_0","name":"Minimal","kind":"allow_once"},
                                    {"optionId":"plan_opt_1","name":"Complete","kind":"allow_once"},
                                    {"optionId":"plan_revise","name":"Revise","kind":"reject_once"},
                                    {"optionId":"plan_reject_and_exit","name":"Reject and Exit","kind":"reject_once"}
                                ]
                            }
                        })).unwrap();
                        stdout.flush().unwrap();
                        continue;
                    }
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update", "params":{
                                "sessionId":"kimi-fixture-session", "update":{
                                    "sessionUpdate":"agent_message_chunk",
                                    "content":{"type":"text","text":"kimi answer"}
                                }
                            }
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update", "params":{
                                "sessionId":"kimi-fixture-session", "update":{
                                    "sessionUpdate":"tool_call", "toolCallId":"kimi-tool",
                                    "title":"Write file", "kind":"edit",
                                    "rawInput":{"path":"src/lib.rs"}
                                }
                            }
                        })
                    )
                    .unwrap();
                    writeln!(stdout, "{}", json!({
                        "jsonrpc":"2.0", "id":"kimi-permission",
                        "method":"session/request_permission", "params":{
                            "sessionId":"kimi-fixture-session",
                            "toolCall":{"toolCallId":"kimi-tool","title":"Write file"},
                            "options":[
                                {"optionId":"approve_once","name":"Approve once","kind":"allow_once"},
                                {"optionId":"approve_always","name":"Approve for this session","kind":"allow_always"},
                                {"optionId":"reject","name":"Reject","kind":"reject_once"}
                            ]
                        }
                    })).unwrap();
                }
                Some("session/resume") => {
                    assert_eq!(frame["params"]["sessionId"], "kimi-existing-session");
                    assert!(frame["params"].get("_meta").is_none());
                    std::fs::write("kimi-resume-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":frame["id"], "result":{
                                "configOptions":kimi_fixture_config_options("model-a", "default")
                            }
                        })
                    )
                    .unwrap();
                }
                Some("session/cancel") => {
                    assert_eq!(frame["params"]["sessionId"], "kimi-fixture-session");
                    std::fs::write("kimi-cancel-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                            "result":{"stopReason":"cancelled"}
                        })
                    )
                    .unwrap();
                }
                None if frame.get("id") == Some(&json!("kimi-permission")) => {
                    assert_eq!(
                        frame
                            .pointer("/result/outcome/optionId")
                            .and_then(Value::as_str),
                        Some("approve_once")
                    );
                    std::fs::write("kimi-permission-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update", "params":{
                                "sessionId":"kimi-fixture-session", "update":{
                                    "sessionUpdate":"tool_call_update", "toolCallId":"kimi-tool",
                                    "status":"completed", "rawOutput":"written"
                                }
                            }
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                            "result":{"stopReason":"end_turn"}
                        })
                    )
                    .unwrap();
                }
                None if frame.get("id") == Some(&json!("kimi-question")) => {
                    assert_eq!(
                        frame.pointer("/result/outcome/optionId"),
                        Some(&json!("q0_opt_1"))
                    );
                    std::fs::write("kimi-question-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                            "result":{"stopReason":"end_turn"}
                        })
                    )
                    .unwrap();
                }
                None if frame.get("id") == Some(&json!("kimi-plan-review")) => {
                    assert_eq!(
                        frame.pointer("/result/outcome/optionId"),
                        Some(&json!("plan_opt_1"))
                    );
                    std::fs::write("kimi-plan-review-observed", b"ok").unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                            "result":{"stopReason":"end_turn"}
                        })
                    )
                    .unwrap();
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn kimi_fixture_drives_standard_auth_config_stream_approval_and_cleanup() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_source_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "model-b",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        assert!(session.negotiated.kimi_source_contract);
        assert!(!session.negotiated.grok_source_contract);
        assert!(session.capabilities().set_model);
        assert!(session.capabilities().set_mode);
        assert!(session.capabilities().set_thinking);
        assert!(matches!(
            session.capabilities().resume,
            ResumeCapability::AcpResume
        ));
        assert_eq!(session.capabilities().subagents, SubagentVisibility::None);

        assert_eq!(
            session.set_thinking(false).await.unwrap(),
            SessionStateUpdate::ThinkingChanged {
                enabled: Some(false),
                can_enable: true,
                can_disable: true,
            }
        );

        session.send_turn("hello kimi".to_string()).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut text = String::new();
        let mut saw_tool = false;
        let mut saw_result = false;
        let mut saw_skill_catalog = false;
        loop {
            let event = tokio::time::timeout_at(deadline, session.next_event())
                .await
                .expect("Kimi fixture turn timed out")
                .expect("Kimi fixture session closed");
            match event {
                SessionEvent::TextDelta(delta) => text.push_str(&delta),
                SessionEvent::ToolCallCorrelated { call_id, .. } => {
                    saw_tool = call_id == "kimi-tool";
                }
                SessionEvent::ToolResultCorrelated { call_id, ok, .. } => {
                    saw_result = call_id == "kimi-tool" && ok;
                }
                SessionEvent::StateUpdate(SessionStateUpdate::CommandCatalogReplaced {
                    commands,
                    ..
                }) => {
                    saw_skill_catalog = commands.iter().any(|command| {
                        command.name == "skill:review"
                            && command.description == "Review the current patch"
                    });
                }
                SessionEvent::HostRequest { req_id, .. } => {
                    session
                        .respond_host(
                            &req_id,
                            HostResponse::Approval {
                                decision: ApprovalDecision::Allow,
                                selected_option_id: Some("approve_once".to_string()),
                                message: None,
                            },
                        )
                        .await
                        .unwrap();
                }
                SessionEvent::TurnDone { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(text, "kimi answer");
        assert!(saw_tool && saw_result);
        assert!(saw_skill_catalog);

        session
            .send_turn("/skill:review --focus src  ".to_string())
            .await
            .unwrap();
        let status = loop {
            let event = tokio::time::timeout(Duration::from_secs(5), session.next_event())
                .await
                .expect("Kimi native skill command timed out")
                .expect("Kimi native skill command closed the session");
            if let SessionEvent::TurnDone { status, .. } = event {
                break status;
            }
        };
        assert_eq!(status, TurnStatus::Completed);
        session.end().await.unwrap();
        for marker in [
            "kimi-auth-observed",
            "kimi-new-standard-observed",
            "kimi-model-observed",
            "kimi-mode-observed",
            "kimi-thinking-observed",
            "kimi-permission-observed",
            "kimi-native-skill-observed",
        ] {
            assert_eq!(std::fs::read(workspace.path().join(marker)).unwrap(), b"ok");
        }
    }

    #[tokio::test]
    async fn kimi_auto_is_locally_mediated_instead_of_prompting_more_than_guarded() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_source_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "model-b",
            BasePermissionProfile::Auto,
            None,
        )
        .await
        .unwrap();
        session.send_turn("hello kimi".to_string()).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline, session.next_event())
                .await
                .expect("Kimi Auto fixture timed out")
                .expect("Kimi Auto fixture closed");
            match event {
                SessionEvent::HostRequest {
                    req_id,
                    request:
                        HostRequest::Approval {
                            metadata, options, ..
                        },
                } => {
                    assert_eq!(metadata["requestedProfile"], "auto");
                    assert_eq!(metadata["locallyMediated"], true);
                    assert!(metadata.get("upstreamPermissionBoundary").is_none());
                    session
                        .respond_host(
                            &req_id,
                            HostResponse::Approval {
                                decision: ApprovalDecision::Allow,
                                selected_option_id: options
                                    .iter()
                                    .find(|option| option.kind == HostApprovalOptionKind::AllowOnce)
                                    .map(|option| option.id.clone()),
                                message: None,
                            },
                        )
                        .await
                        .unwrap();
                }
                SessionEvent::TurnDone { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        session.end().await.unwrap();
    }

    #[tokio::test]
    async fn kimi_ask_user_permission_bridge_surfaces_a_real_question() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_source_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "model-b",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        session.send_turn("ask me".to_string()).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut answered = false;
        loop {
            let event = tokio::time::timeout_at(deadline, session.next_event())
                .await
                .expect("Kimi question fixture timed out")
                .expect("Kimi question fixture closed");
            match event {
                SessionEvent::HostRequest {
                    req_id,
                    request:
                        HostRequest::UserInput {
                            questions,
                            metadata,
                        },
                } => {
                    assert_eq!(metadata["transport"], "kimi_permission_question_v1");
                    assert_eq!(questions.len(), 1);
                    assert_eq!(questions[0].prompt, "Which implementation?");
                    assert_eq!(questions[0].options[1].label, "Complete");
                    assert_eq!(questions[0].options[1].value, "q0_opt_1");
                    session
                        .respond_host(
                            &req_id,
                            HostResponse::UserInput {
                                answers: vec![HostAnswer {
                                    question_id: questions[0].id.clone(),
                                    values: vec!["q0_opt_1".to_string()],
                                }],
                            },
                        )
                        .await
                        .unwrap();
                    answered = true;
                }
                SessionEvent::TurnDone { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                SessionEvent::HostRequest {
                    request: HostRequest::Approval { .. },
                    ..
                } => panic!("Kimi AskUserQuestion was misclassified as approval"),
                _ => {}
            }
        }
        assert!(answered);
        session.end().await.unwrap();
        assert_eq!(
            std::fs::read(workspace.path().join("kimi-question-observed")).unwrap(),
            b"ok"
        );
    }

    #[tokio::test]
    async fn kimi_plan_review_requires_and_round_trips_the_exact_user_choice() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_source_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "model-b",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        session.send_turn("review plan".to_string()).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut answered = false;
        loop {
            let event = tokio::time::timeout_at(deadline, session.next_event())
                .await
                .expect("Kimi plan review fixture timed out")
                .expect("Kimi plan review fixture closed");
            match event {
                SessionEvent::HostRequest {
                    req_id,
                    request:
                        HostRequest::UserInput {
                            questions,
                            metadata,
                        },
                } => {
                    assert_eq!(
                        metadata["responseContract"],
                        "kimi_plan_review_permission_v1"
                    );
                    assert_eq!(questions.len(), 1);
                    assert!(questions[0].prompt.contains("# Delivery plan"));
                    assert_eq!(questions[0].options.len(), 4);
                    assert_eq!(questions[0].options[1].label, "Complete");
                    assert_eq!(questions[0].options[1].value, "plan_opt_1");
                    session
                        .respond_host(
                            &req_id,
                            HostResponse::UserInput {
                                answers: vec![HostAnswer {
                                    question_id: questions[0].id.clone(),
                                    values: vec!["plan_opt_1".to_string()],
                                }],
                            },
                        )
                        .await
                        .unwrap();
                    answered = true;
                }
                SessionEvent::TurnDone { status, .. } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                SessionEvent::HostRequest {
                    request: HostRequest::Approval { .. },
                    ..
                } => panic!("Kimi plan review was misclassified as binary approval"),
                _ => {}
            }
        }
        assert!(answered);
        session.end().await.unwrap();
        assert_eq!(
            std::fs::read(workspace.path().join("kimi-plan-review-observed")).unwrap(),
            b"ok"
        );
    }

    #[tokio::test]
    async fn kimi_fixture_resumes_without_replay_and_reapplies_the_permission_mode() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_source_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            Some("kimi-existing-session"),
        )
        .await
        .unwrap();
        assert_eq!(session.session_id(), Some("kimi-existing-session"));
        assert_eq!(session.model, "model-a");
        session.end().await.unwrap();
        for marker in [
            "kimi-auth-observed",
            "kimi-resume-observed",
            "kimi-mode-observed",
        ] {
            assert_eq!(std::fs::read(workspace.path().join(marker)).unwrap(), b"ok");
        }
        assert!(!workspace.path().join("kimi-new-standard-observed").exists());
    }

    #[tokio::test]
    async fn kimi_fixture_cancel_is_idempotent_and_waits_for_the_turn_terminal() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_kimi_source_contract_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Kimi,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        session.send_turn("cancel me".to_string()).await.unwrap();
        session.interrupt().await.unwrap();
        assert!(matches!(
            session.next_event().await,
            Some(SessionEvent::StateUpdate(_) | SessionEvent::SessionModel(_))
        ));
        // Repeated cancellation after the terminal is a local no-op.
        session.interrupt().await.unwrap();
        session.end().await.unwrap();
        for marker in ["kimi-cancellable-turn-observed", "kimi-cancel-observed"] {
            assert_eq!(std::fs::read(workspace.path().join(marker)).unwrap(), b"ok");
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fake_acp_child() {
        let invoked_as_fixture = std::env::args().any(|arg| arg == "--exact")
            && std::env::args().any(|arg| arg.ends_with("fake_acp_child"));
        if !invoked_as_fixture {
            return;
        }
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout();
        // libtest prints `test <name> ... ` without a newline before invoking
        // the body. Terminate that harness prefix so every protocol frame still
        // begins at column zero and the ACP reader can parse it independently.
        writeln!(stdout).unwrap();
        stdout.flush().unwrap();
        let mut prompt_id = None;
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match frame.get("method").and_then(Value::as_str) {
                Some("initialize") => {
                    let response = json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{"loadSession":true},
                            "agentInfo":{"name":"fake-grok","version":"1.0.0"}
                        }
                    });
                    writeln!(stdout, "{response}").unwrap();
                }
                Some("session/new") => {
                    let response = json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"sessionId":"fixture-session","model":"fixture-model"}
                    });
                    writeln!(stdout, "{response}").unwrap();
                }
                Some("session/load") => {
                    let response = json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{"model":"fixture-resumed-model"}
                    });
                    writeln!(stdout, "{response}").unwrap();
                }
                Some("session/prompt") => {
                    prompt_id = frame.get("id").cloned();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update",
                            "params":{"sessionId":"fixture-session","update":{
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":"fixture answer"}
                            }}
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update",
                            "params":{"sessionId":"fixture-session","update":{
                                "sessionUpdate":"usage_update", "used":14, "size":100_000,
                                "inputTokens":11, "outputTokens":3
                            }}
                        })
                    )
                    .unwrap();
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update",
                            "params":{"sessionId":"fixture-session","update":{
                                "sessionUpdate":"tool_call", "toolCallId":"tool-1",
                                "kind":"read", "title":"Read file",
                                "rawInput":{"path":"src/lib.rs"}
                            }}
                        })
                    )
                    .unwrap();
                    writeln!(stdout, "{}", json!({
                        "jsonrpc":"2.0", "id":"permission-1",
                        "method":"session/request_permission",
                        "params":{
                            "sessionId":"fixture-session",
                            "toolCall":{"toolCallId":"tool-1","kind":"read","title":"Read file","rawInput":{"path":"src/lib.rs"}},
                            "options":[
                                {"optionId":"allow-once","name":"Allow","kind":"allow_once"},
                                {"optionId":"reject-once","name":"Reject","kind":"reject_once"}
                            ]
                        }
                    }))
                    .unwrap();
                }
                None if frame.get("id") == Some(&json!("permission-1")) => {
                    assert_eq!(
                        frame
                            .pointer("/result/outcome/optionId")
                            .and_then(Value::as_str),
                        Some("allow-once")
                    );
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "id":"private-1", "method":"_x.ai/future_request",
                            "params":{"apiKey":"must-not-leak", "visible":"diagnostic"}
                        })
                    )
                    .unwrap();
                }
                None if frame.get("id") == Some(&json!("private-1")) => {
                    assert_eq!(
                        frame.pointer("/error/code").and_then(Value::as_i64),
                        Some(-32_601)
                    );
                    writeln!(
                        stdout,
                        "{}",
                        json!({
                            "jsonrpc":"2.0", "method":"session/update",
                            "params":{"sessionId":"fixture-session","update":{
                                "sessionUpdate":"tool_call_update", "toolCallId":"tool-1",
                                "status":"completed", "rawOutput":"read complete"
                            }}
                        })
                    )
                    .unwrap();
                    let response = json!({
                        "jsonrpc":"2.0", "id":prompt_id.take().unwrap(),
                        "result":{"stopReason":"end_turn"}
                    });
                    writeln!(stdout, "{response}").unwrap();
                }
                _ => {}
            }
            stdout.flush().unwrap();
        }
    }

    #[tokio::test]
    async fn fake_acp_fixture_exercises_handshake_permission_stream_and_cleanup() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            None,
        )
        .await
        .unwrap();
        assert_eq!(session.session_id(), Some("fixture-session"));
        session.send_turn("hello".to_string()).await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut text = String::new();
        let mut saw_model = false;
        let mut saw_call = false;
        let mut saw_result = false;
        let mut saw_private_request = false;
        let usage = loop {
            let event = tokio::time::timeout_at(deadline, session.next_event())
                .await
                .expect("fixture turn timed out")
                .expect("fixture session closed");
            match event {
                SessionEvent::SessionModel(model) => saw_model = model == "fixture-model",
                SessionEvent::TextDelta(delta) => text.push_str(&delta),
                SessionEvent::ToolCallCorrelated { call_id, .. } => {
                    saw_call = call_id == "tool-1";
                }
                SessionEvent::ToolResultCorrelated { call_id, ok, .. } => {
                    saw_result = call_id == "tool-1" && ok;
                }
                SessionEvent::HostRequest {
                    req_id,
                    request: HostRequest::Approval { .. },
                } => {
                    session
                        .respond_host(
                            &req_id,
                            HostResponse::Approval {
                                decision: ApprovalDecision::Allow,
                                selected_option_id: None,
                                message: None,
                            },
                        )
                        .await
                        .unwrap();
                }
                SessionEvent::HostRequest {
                    request: HostRequest::Unknown { method, payload },
                    ..
                } => {
                    saw_private_request = method == "x.ai/future_request"
                        && payload.get("apiKey") == Some(&json!("[redacted]"))
                        && payload.get("visible") == Some(&json!("diagnostic"));
                }
                SessionEvent::TurnDone { status, usage } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break usage;
                }
                _ => {}
            }
        };
        assert_eq!(text, "fixture answer");
        assert!(saw_model && saw_call && saw_result && saw_private_request);
        assert_eq!(
            usage, None,
            "unscoped usage_update is not whole-prompt usage"
        );
        session.end().await.unwrap();
    }

    #[tokio::test]
    async fn fake_acp_fixture_exercises_session_load_and_resumed_model() {
        let executable = std::env::current_exe().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let args = vec![
            "--exact".to_string(),
            "acp::tests::fake_acp_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        let mut session = AcpSession::start_with_program_args(
            AcpVendor::Grok,
            executable.to_str().unwrap(),
            args,
            workspace.path(),
            "",
            BasePermissionProfile::Guarded,
            Some("existing-session"),
        )
        .await
        .unwrap();
        assert_eq!(session.session_id(), Some("existing-session"));
        assert_eq!(
            session.next_event().await,
            Some(SessionEvent::SessionModel(
                "fixture-resumed-model".to_string()
            ))
        );
        session.end().await.unwrap();
    }

    #[test]
    fn prompt_stop_reasons_map_without_guessing() {
        assert_eq!(
            parse_prompt_result(&json!({"stopReason":"end_turn"})).0,
            TurnStatus::Completed
        );
        assert_eq!(
            parse_prompt_result(&json!({"stopReason":"max_tokens"})).0,
            TurnStatus::Truncated
        );
        assert_eq!(
            parse_prompt_result(&json!({"stopReason":"cancelled"})).0,
            TurnStatus::Interrupted
        );
        assert!(matches!(
            parse_prompt_result(&json!({"stopReason":"future"})).0,
            TurnStatus::Failed(_)
        ));
        assert!(matches!(
            parse_prompt_result(&json!({})).0,
            TurnStatus::Failed(reason) if reason.contains("omitted")
        ));
        assert!(matches!(
            parse_prompt_result(&json!({"stopReason":""})).0,
            TurnStatus::Failed(reason) if reason.contains("empty")
        ));
    }

    #[test]
    fn malformed_rpc_responses_and_wrong_session_ids_never_claim_success() {
        assert_eq!(
            acp_response_payload(&json!({"id":1,"result":{"ok":true}})).unwrap(),
            json!({"ok":true})
        );
        assert!(acp_response_payload(&json!({"id":1})).is_err());
        assert!(acp_response_payload(&json!({
            "id":1,
            "result":{},
            "error":{"code":-1,"message":"bad"}
        }))
        .is_err());
        assert!(acp_response_payload(&json!({
            "jsonrpc":"1.0", "id":1, "result":{}
        }))
        .is_err());

        let active: ActiveSessionId = Arc::new(RwLock::new(Some("session-live".to_string())));
        assert!(acp_message_matches_active_session(
            &json!({"extra":true}),
            &active
        ));
        assert!(acp_message_matches_active_session(
            &json!({"sessionId":"session-live"}),
            &active
        ));
        assert!(!acp_message_matches_active_session(
            &json!({"sessionId":"session-stale"}),
            &active
        ));
    }
}
