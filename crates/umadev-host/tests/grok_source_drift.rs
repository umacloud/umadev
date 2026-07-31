use std::path::{Path, PathBuf};

use umadev_host::grok_contract::{
    GROK_BUILD_SOURCE_ACP_SCHEMA_VERSION, GROK_BUILD_SOURCE_ACP_VERSION, GROK_BUILD_SOURCE_COMMIT,
    GROK_BUILD_SOURCE_VERSION,
};

fn source_root() -> Option<PathBuf> {
    std::env::var_os("UMADEV_GROK_SOURCE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn read(root: &Path, relative: &str) -> String {
    std::fs::read_to_string(root.join(relative))
        .unwrap_or_else(|error| panic!("read audited Grok source {relative}: {error}"))
}

fn source_head(root: &Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|error| panic!("read audited Grok source commit: {error}"));
    assert!(
        output.status.success(),
        "git rev-parse failed for audited Grok source: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("audited Grok source commit is UTF-8")
        .trim()
        .to_string()
}

fn assert_markers(source: &str, contract: &str, markers: &[&str]) {
    for marker in markers {
        assert!(
            source.contains(marker),
            "missing audited {contract} marker {marker}"
        );
    }
}

#[test]
fn audited_grok_baseline_matches_the_wire_contract() {
    let Some(root) = source_root() else {
        eprintln!("skipped: UMADEV_GROK_SOURCE_DIR is set by the source-contract CI job");
        return;
    };

    assert_eq!(source_head(&root), GROK_BUILD_SOURCE_COMMIT);

    let workspace = read(&root, "Cargo.toml");
    assert!(workspace.contains(&format!(
        "agent-client-protocol = {{ version = \"{GROK_BUILD_SOURCE_ACP_VERSION}\""
    )));

    for manifest in [
        "crates/codegen/xai-grok-pager/Cargo.toml",
        "crates/codegen/xai-grok-pager-bin/Cargo.toml",
        "crates/codegen/xai-grok-shell/Cargo.toml",
    ] {
        assert!(
            read(&root, manifest).contains(&format!("version = \"{GROK_BUILD_SOURCE_VERSION}\""))
        );
    }

    let lock = read(&root, "Cargo.lock");
    assert!(lock.contains(&format!(
        "name = \"agent-client-protocol-schema\"\nversion = \"{GROK_BUILD_SOURCE_ACP_SCHEMA_VERSION}\""
    )));

    let auth = read(
        &root,
        "crates/codegen/xai-grok-shell/src/extensions/auth.rs",
    );
    for method in [
        "x.ai/auth/get_url",
        "x.ai/auth/submit_code",
        "x.ai/auth/cancel",
    ] {
        assert!(
            auth.contains(method),
            "missing audited auth method {method}"
        );
    }
    assert!(
        auth.contains("let rx = agent.interactive_auth.take_url_rx()"),
        "auth URL is no longer read from the generation-bound one-shot receiver; re-audit the bootstrap poller"
    );

    let single_flight = read(
        &root,
        "crates/codegen/xai-grok-shell/src/auth/single_flight.rs",
    );
    for marker in [
        "Single-flight guard for interactive login.",
        "x.ai/auth/cancel",
        "pub(crate) fn take_url_rx",
        "ch.url_rx.take()",
        "pub(crate) fn cancel_for_client_seq",
    ] {
        assert!(
            single_flight.contains(marker),
            "missing audited generation-bound authentication marker {marker}"
        );
    }

    // Interactive auth is explicitly user-authorized in UmaDev because this
    // pinned Grok build opens browsers itself. Lock both the loopback ordering
    // (browser open before URL delivery) and device ordering (URL delivery
    // before detached browser open); neither behavior may be guessed after a
    // source bump.
    let oidc_login = read(
        &root,
        "crates/codegen/xai-grok-shell/src/auth/oidc/login.rs",
    );
    let loopback_open = oidc_login
        .find("webbrowser::open(&auth_url)")
        .expect("missing audited loopback browser open");
    let loopback_send = oidc_login
        .find("if let Some(tx) = url_tx")
        .expect("missing audited loopback URL delivery");
    assert!(
        loopback_open < loopback_send,
        "loopback browser/URL order changed; re-audit interactive auth UX"
    );

    let device_login = read(
        &root,
        "crates/codegen/xai-grok-shell/src/auth/device_code.rs",
    );
    let device_send = device_login
        .find("if let Some(tx) = channels.url_tx")
        .expect("missing audited device URL delivery");
    let device_open = device_login
        .find("open_browser_detached(&display_uri).await")
        .expect("missing audited device browser open");
    assert!(
        device_send < device_open,
        "device browser/URL order changed; re-audit interactive auth UX"
    );

    let pager_effects = read(
        &root,
        "crates/codegen/xai-grok-pager/src/app/effects/mod.rs",
    );
    assert!(pager_effects.contains("for i in 0..60"));
    assert!(pager_effects.contains("Duration::from_millis(50)"));

    // The audited Linux read-only launch protects Grok's direct hook sources
    // with a mount namespace before ACP starts.  That happens before
    // `initialize`, so the published-binary job must provide a usable bwrap;
    // otherwise an auth contract probe observes only an unexplained EOF.
    let sandbox_startup = read(&root, "crates/codegen/xai-grok-shell/src/config/mod.rs");
    assert_markers(
        &sandbox_startup,
        "Linux pre-ACP sandbox prerequisite",
        &[
            "let requires_hook_write_deny =",
            "bwrap_reexec_for_profile(&sandbox_profile, &workspace)",
            "Install bubblewrap with",
            "std::process::exit(1)",
        ],
    );
    let hook_write_deny = read(
        &root,
        "crates/codegen/xai-grok-sandbox/src/hook_write_deny.rs",
    );
    assert!(hook_write_deny.contains("!matches!(profile, ProfileName::Devbox | ProfileName::Off)"));

    let folder_trust = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/mvp_agent/folder_trust_prompt.rs",
    );
    for marker in [
        "x.ai/folder_trust/request",
        "x.ai/folderTrust",
        "interactive",
        "TRUST_PROMPT_TIMEOUT",
    ] {
        assert!(
            folder_trust.contains(marker),
            "missing audited Folder Trust marker {marker}"
        );
    }

    let permission_prompter = read(
        &root,
        "crates/codegen/xai-grok-workspace/src/permission/prompter.rs",
    );
    for marker in [
        "acp::PermissionOptionKind::AllowOnce",
        "acp::PermissionOptionKind::AllowAlways",
        "acp::PermissionOptionKind::RejectOnce",
        "acp::PermissionOptionKind::RejectAlways",
        "RequestPermissionOutcome::Cancelled",
    ] {
        assert!(
            permission_prompter.contains(marker),
            "missing audited permission marker {marker}"
        );
    }

    // UmaDev deliberately does not re-authenticate an advertised current token.
    // Grok now gives an expired initialize-time token one bounded refresh attempt;
    // a current token remains selected without an extra interactive auth request.
    // Keep both sides of that source dependency pinned.
    let acp_agent = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs",
    );
    for marker in [
        "let mut has_cached_token = init_has_current",
        "crate::http::STARTUP_AUTH_REFRESH_TIMEOUT",
        "self.auth_manager.auth()",
        "Ok(Ok(_))",
        "self.set_auth_method(default_id)",
        "self.seed_client_config_auth_if_available()",
    ] {
        assert!(
            acp_agent.contains(marker),
            "missing audited cached-auth marker {marker}"
        );
    }
    let agent_ops = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs",
    );
    assert!(agent_ops.contains("\"use_oauth\": true"));

    let updates = read(
        &root,
        "crates/codegen/xai-grok-shell/src/extensions/notification.rs",
    );
    for update in [
        "TaskBackgrounded",
        "TaskCompleted",
        "subagent_spawned",
        "subagent_progress",
        "subagent_finished",
        "TurnCompleted",
    ] {
        assert!(updates.contains(update), "missing audited update {update}");
    }

    let slash = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/slash_commands.rs",
    );
    assert!(slash.contains("trimmed.strip_prefix('/')"));
    assert!(read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs"
    )
    .contains("slash_commands::resolve"));

    let wire_tags = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/wire_tags.rs",
    );
    assert!(wire_tags.contains("available_commands_update"));
    assert!(read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/acp_session_impl/updates.rs"
    )
    .contains("SessionUpdate::CurrentModeUpdate"));
    assert!(read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/turn_completion.rs"
    )
    .contains("TurnCompleted"));

    // Standard session control stays on ACP while Grok's richer prompt queue,
    // steering and background-task controls remain typed private extensions.
    // Pin both sides so a source bump cannot leave UmaDev negotiating a method
    // whose handler or required fields disappeared upstream.
    assert_markers(
        &acp_agent,
        "standard session lifecycle",
        &[
            "async fn new_session(",
            "async fn load_session(",
            "async fn prompt(",
            "async fn cancel(&self, args: acp::CancelNotification)",
            "async fn set_session_mode(",
            "async fn set_session_model(",
            "\"x.ai/interject\" => crate::extensions::interject::handle",
            "\"x.ai/queue/remove\"",
            "| \"x.ai/queue/reorder\"",
            "| \"x.ai/queue/clear\"",
            "| \"x.ai/queue/edit\"",
            "| \"x.ai/queue/interject\"",
        ],
    );

    let interject = read(
        &root,
        "crates/codegen/xai-grok-shell/src/extensions/interject.rs",
    );
    assert_markers(
        &interject,
        "interject request",
        &[
            "struct InterjectRequest",
            "session_id: String",
            "text: String",
            "interjection_id: Option<String>",
            "content: Vec<acp::ContentBlock>",
            "SessionCommand::Interject",
            "\"status\": \"queued\"",
        ],
    );

    let queue = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/ext_parsers.rs",
    );
    assert_markers(
        &queue,
        "versioned prompt queue",
        &[
            "\"x.ai/queue/remove\"",
            "\"expectedVersion\"",
            "SessionCommand::RemoveQueuedPrompt",
            "\"x.ai/queue/reorder\"",
            "SessionCommand::ReorderQueue",
            "\"x.ai/queue/clear\"",
            "SessionCommand::ClearQueue",
            "\"x.ai/queue/edit\"",
            "SessionCommand::EditQueuedPrompt",
            "\"x.ai/queue/interject\"",
            "SessionCommand::InterjectQueuedPrompt",
        ],
    );
    assert!(read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/prompt_queue.rs"
    )
    .contains("pub const QUEUE_CHANGED_METHOD: &str = \"x.ai/queue/changed\""));

    let tasks = read(
        &root,
        "crates/codegen/xai-grok-shell/src/extensions/task.rs",
    );
    assert_markers(
        &tasks,
        "background task control",
        &[
            "\"x.ai/task/kill\"",
            ".kill_background_task(&req.session_id, &req.task_id)",
            "\"x.ai/task/list\"",
            ".list_tasks(&req.session_id)",
        ],
    );

    let tool_calls = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs",
    );
    assert_markers(
        &tool_calls,
        "standard tool lifecycle",
        &[
            "acp::SessionUpdate::ToolCall(",
            "acp::ToolCallStatus::Pending",
            "acp::SessionUpdate::ToolCallUpdate(",
            "acp::ToolCallStatus::Completed",
            "acp::ToolCallStatus::Failed",
        ],
    );
}

#[test]
fn audited_grok_baseline_matches_the_subagent_contract() {
    let Some(root) = source_root() else {
        eprintln!("skipped: UMADEV_GROK_SOURCE_DIR is set by the source-contract CI job");
        return;
    };

    assert_eq!(source_head(&root), GROK_BUILD_SOURCE_COMMIT);

    // Lifecycle notifications are carried on the parent session while naming
    // the child session that subsequent child-scoped traffic belongs to.
    let notification = read(
        &root,
        "crates/codegen/xai-grok-shell/src/extensions/notification.rs",
    );
    assert_markers(
        &notification,
        "subagent lifecycle wire",
        &[
            "pub struct SessionNotification",
            "pub session_id: acp::SessionId",
            "SubagentSpawned {",
            "parent_session_id: String",
            "parent_prompt_id: Option<String>",
            "child_session_id: String",
            "SubagentProgress {",
            "context_window_tokens: u64",
            "context_usage_pct: u8",
            "SubagentFinished {",
            "status: String",
            "will_wake: bool",
        ],
    );

    // The parent-to-child route must be announced before the child can emit a
    // prompt-side update or reverse request.
    let subagent_request = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/subagent/handle_request.rs",
    );
    let spawned = subagent_request
        .find("SessionUpdate::SubagentSpawned {")
        .expect("missing audited SubagentSpawned emission");
    let child_prompt = subagent_request[spawned..]
        .find(".send(SessionCommand::Prompt {")
        .map(|offset| spawned + offset)
        .expect("missing audited child SessionCommand::Prompt dispatch");
    assert!(
        spawned < child_prompt,
        "child prompt moved before SubagentSpawned; re-audit child-session routing"
    );
    assert_markers(
        &subagent_request[spawned..child_prompt],
        "spawn-before-child-prompt",
        &[
            "child_session_id: child_session_id.0.to_string()",
            "parent_session_id: ctx.parent_session_id.clone()",
        ],
    );

    // Blocking child interactions use the child actor's own session ID. These
    // are legitimate child-scoped requests, not foreign-session traffic.
    let spawn = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/acp_session_impl/spawn.rs",
    );
    assert_markers(
        &spawn,
        "child ask-user wire",
        &[
            "let session_id = session.session_info.id.clone()",
            "session_id: session_id.0.to_string()",
            "\"x.ai/ask_user_question\"",
        ],
    );
    let tool_calls = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs",
    );
    assert_markers(
        &tool_calls,
        "child exit-plan wire",
        &[
            "session_id: self.session_id_string()",
            "\"x.ai/exit_plan_mode\"",
        ],
    );

    // Durable lifecycle edges are event-ID stamped. Replay tracks both the
    // event cursor and unpaired (subagent, child-session) spawns so restart
    // recovery can emit the missing terminal edge without duplicating history.
    let subagent = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/subagent/mod.rs",
    );
    assert_markers(
        &subagent,
        "subagent event identity",
        &[
            "ensure_event_id_meta(parent_session_id, &mut meta)",
            "acp::ExtNotification::new(\"x.ai/session_notification\"",
        ],
    );
    let storage = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/storage/mod.rs",
    );
    assert_markers(
        &storage,
        "subagent replay",
        &[
            "pub(crate) const XAI_SESSION_UPDATE_METHOD: &str = \"_x.ai/session/update\"",
            "pub(crate) max_event_seq: Option<u64>",
            "pub(crate) unfinished_subagents: Vec<(String, String)>",
            "fn collect_unfinished_subagents",
            "Update::SubagentSpawned {",
            "child_session_id",
            "Update::SubagentFinished { subagent_id, .. }",
            "filtered.iter().rposition(|l| line_has_event_id(l, id))",
            "unfinished_subagents: collect_unfinished_subagents(&filtered)",
        ],
    );

    // A completion marked will_wake is followed by one synthetic parent prompt
    // with a stable ID. Clients must not race it with their own recovery turn.
    assert_markers(
        &subagent,
        "subagent auto-wake",
        &[
            "fn should_auto_wake_subagent(",
            "&& !block_waited",
            "&& !explicitly_killed",
            "&& parent_channel_open",
            "let prompt_id = format!(\"subagent-completed-{subagent_id}\")",
            "prompt_id: prompt_id.clone()",
        ],
    );
    let finished = subagent
        .find("SessionUpdate::SubagentFinished {")
        .expect("missing audited SubagentFinished emission");
    let wake = subagent[finished..]
        .find("if will_wake {")
        .map(|offset| finished + offset)
        .expect("missing audited will_wake injection gate");
    assert_markers(
        &subagent[finished..wake],
        "will_wake finish wire",
        &["will_wake,", "completion_data.parent_cmd_tx.as_ref()"],
    );

    // Progress is transient rather than replayed. Reconnect obtains an
    // authoritative parent-scoped live snapshot through list_running.
    let task_extension = read(
        &root,
        "crates/codegen/xai-grok-shell/src/extensions/task.rs",
    );
    assert_markers(
        &task_extension,
        "subagent list-running resync",
        &[
            "struct ListRunningSubagentsRequest",
            "session_id: String",
            "struct ListRunningSubagentsResponse",
            "subagents: Vec<SubagentLiveSnapshotDto>",
            "\"x.ai/subagent/list_running\"",
            ".list_running_subagents(&req.session_id)",
        ],
    );
    let agent_ops = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/mvp_agent/agent_ops.rs",
    );
    assert_markers(
        &agent_ops,
        "subagent coordinator resync",
        &[
            "pub(crate) async fn list_running_subagents(",
            ".list_running(parent_session_id)",
        ],
    );
    let updates = read(
        &root,
        "crates/codegen/xai-grok-shell/src/session/acp_session_impl/updates.rs",
    );
    let progress = updates
        .find("XaiSessionUpdate::SubagentProgress {")
        .expect("missing audited SubagentProgress branch");
    let progress_return = updates[progress..]
        .find("return;")
        .map(|offset| progress + offset)
        .expect("SubagentProgress no longer exits before persistence");
    let persistence = updates[progress..]
        .find(".persistence_tx")
        .map(|offset| progress + offset)
        .expect("missing audited xAI persistence send");
    assert!(
        progress_return < persistence,
        "SubagentProgress is now persisted; re-audit replay/resync semantics"
    );

    // ACP cancel omits this private flag in normal clients; the pinned agent
    // defaults it to true and forwards that value into SessionCommand::Cancel.
    let acp_agent = read(
        &root,
        "crates/codegen/xai-grok-shell/src/agent/mvp_agent/acp_agent.rs",
    );
    let cancel_key = acp_agent
        .find(".get(\"cancelSubagents\")")
        .expect("missing audited cancelSubagents metadata key");
    let cancel_default = acp_agent[cancel_key..]
        .find(".unwrap_or(true)")
        .map(|offset| cancel_key + offset)
        .expect("cancelSubagents no longer defaults to true");
    let cancel_send = acp_agent[cancel_default..]
        .find("SessionCommand::Cancel {")
        .map(|offset| cancel_default + offset)
        .expect("missing audited SessionCommand::Cancel forwarding");
    assert!(cancel_key < cancel_default && cancel_default < cancel_send);
    assert_markers(
        &acp_agent[cancel_send..],
        "cancel-subagents forwarding",
        &["cancel_subagents,", "kill_background_tasks: false"],
    );
}
