//! Opt-in acceptance test against an installed, authenticated first-class CLI.
//!
//! The ordinary suite uses deterministic protocol fixtures. This test is the
//! complementary release-candidate check for the actual vendor process and
//! account on a maintainer machine. It is ignored by default because it makes
//! real model calls and depends on credentials outside the repository.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use tokio::time::{timeout, timeout_at, Instant};
use umadev_host::folder_trust::FolderTrustClientSurface;
use umadev_host::session_bootstrap::SessionOpenPolicy;
use umadev_runtime::{
    ApprovalDecision, BackgroundProcessSignal, BackgroundProcessStopOutcome, BasePermissionProfile,
    BaseSession, HostFolderTrustDecision, HostPlanOutcome, HostRequest, HostResponse,
    PromptQueuePlacement, SessionCapability, SessionEvent, TurnInput, TurnStatus,
};

const OPEN_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_TIMEOUT: Duration = Duration::from_secs(180);

fn selected_backend() -> String {
    let backend = std::env::var("UMADEV_LIVE_BASE")
        .expect("set UMADEV_LIVE_BASE to one of claude-code/codex/opencode/grok-build/kimi-code");
    assert!(
        umadev_host::BACKEND_IDS.contains(&backend.as_str()),
        "unknown live base `{backend}`"
    );
    backend
}

fn workspace_snapshot(root: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, current: &Path, out: &mut Vec<PathBuf>) {
        let mut entries = std::fs::read_dir(current)
            .expect("read live-probe workspace")
            .map(|entry| entry.expect("read live-probe entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            out.push(
                path.strip_prefix(root)
                    .expect("path under root")
                    .to_path_buf(),
            );
            if entry
                .file_type()
                .expect("read live-probe file type")
                .is_dir()
            {
                visit(root, &path, out);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

async fn run_expected_read_only_turn(
    session: &mut dyn BaseSession,
    prompt: String,
    expected: &str,
) -> String {
    session
        .send_turn(prompt)
        .await
        .expect("send live acceptance turn");

    let deadline = Instant::now() + TURN_TIMEOUT;
    let mut text = String::new();
    loop {
        let event = timeout_at(deadline, session.next_event())
            .await
            .expect("live base turn exceeded its acceptance deadline")
            .expect("live base session ended before TurnDone");
        match event {
            SessionEvent::TextDelta(delta) => text.push_str(&delta),
            SessionEvent::NeedApproval { req_id, .. } => session
                .respond(&req_id, ApprovalDecision::Deny)
                .await
                .expect("deny unexpected live-base approval request"),
            SessionEvent::HostRequest { req_id, request } => {
                let response = match &request {
                    HostRequest::PlanConfirmation { metadata, .. }
                        if metadata
                            .get("responseContract")
                            .and_then(serde_json::Value::as_str)
                            == Some("grok_exit_plan_mode_v1") =>
                    {
                        // Leave Plan without granting implementation authority.
                        // Grok uses this internal control even for a text-only
                        // Plan turn; `Abandoned` is explicitly non-mutating.
                        HostResponse::PlanOutcome {
                            outcome: HostPlanOutcome::Abandoned,
                        }
                    }
                    _ => request.safe_rejection(
                        "release acceptance probes never grant authority or answer interactions",
                    ),
                };
                session
                    .respond_host(&req_id, response)
                    .await
                    .expect("reject unexpected live-base host request");
            }
            SessionEvent::ToolCall { name, .. } | SessionEvent::ToolCallCorrelated { name, .. } => {
                assert_eq!(
                    name, "exit_plan_mode",
                    "read-only acceptance turn unexpectedly called tool `{name}`"
                );
            }
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            } => break,
            SessionEvent::TurnDone { status, .. } => {
                panic!("live acceptance turn did not complete cleanly: {status:?}");
            }
            _ => {}
        }
    }

    assert!(
        text.trim() == expected,
        "live base assistant output was not the exact requested correlation token: {text:?}"
    );
    text
}

async fn run_read_only_turn(session: &mut dyn BaseSession, token: &str) -> String {
    run_expected_read_only_turn(
        session,
        format!(
            "This is a read-only transport acceptance check. Do not call tools and do not modify files. Reply with exactly this token and nothing else: {token}"
        ),
        token,
    )
    .await
}

async fn run_full_access_turn(session: &mut dyn BaseSession, token: &str) {
    session
        .send_turn(format!(
            "Perform this isolated development acceptance task using real tools. First create `write-probe.txt` with exactly `{token}` and one trailing newline. Then run exactly `cargo run --quiet --manifest-path port-probe/Cargo.toml`. The supplied Rust program binds an ephemeral 127.0.0.1 port, connects to it, and writes `port-probe.ok` only after the round trip succeeds. Do not modify any other source file. Finish with a brief confirmation."
        ))
        .await
        .expect("send live full-access turn");

    let deadline = Instant::now() + TURN_TIMEOUT;
    let mut saw_tool = false;
    let mut saw_port_command = false;
    let mut tool_evidence = Vec::new();
    loop {
        let event = timeout_at(deadline, session.next_event())
            .await
            .expect("live full-access turn exceeded its acceptance deadline")
            .expect("live full-access session ended before TurnDone");
        match event {
            SessionEvent::ToolCall { name, input }
            | SessionEvent::ToolCallCorrelated { name, input, .. } => {
                saw_tool = true;
                let input = input.to_string();
                if input.contains("cargo run") {
                    saw_port_command = true;
                }
                tool_evidence.push(format!("{name}: {input}"));
            }
            SessionEvent::NeedApproval { action, target, .. } => {
                panic!("Auto profile unexpectedly requested approval for {action}: {target}");
            }
            SessionEvent::HostRequest { request, .. } => {
                panic!("Auto profile unexpectedly requested host input: {request:?}");
            }
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            } => break,
            SessionEvent::TurnDone { status, .. } => {
                panic!("live full-access turn did not complete cleanly: {status:?}");
            }
            _ => {}
        }
    }
    assert!(
        saw_tool,
        "full-access acceptance observed no real tool call"
    );
    assert!(
        saw_port_command,
        "full-access acceptance did not observe the required cargo-run tool call: {tool_evidence:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "makes real model calls; set UMADEV_LIVE_BASE and run explicitly"]
async fn installed_base_opens_turns_and_forks_without_writing() {
    let backend = selected_backend();
    let workspace = TempDir::new().expect("create isolated live-probe workspace");
    std::fs::write(
        workspace.path().join("sentinel.txt"),
        "UmaDev live base acceptance sentinel\n",
    )
    .expect("write live-probe sentinel");
    let before = workspace_snapshot(workspace.path());

    let mut session = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_with_policy(
            &backend,
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            Some("Remain read-only. This is a release transport acceptance check."),
            SessionOpenPolicy::NonInteractive,
        ),
    )
    .await
    .expect("live base did not finish opening in time")
    .unwrap_or_else(|error| panic!("failed to open authenticated `{backend}` session: {error}"));

    let parent_token = format!("UMADEV_LIVE_PARENT_{}", std::process::id());
    let parent_text = run_read_only_turn(session.as_mut(), &parent_token).await;
    eprintln!("{backend}: parent turn accepted: {}", parent_text.trim());

    let mut critic = timeout(OPEN_TIMEOUT, session.fork())
        .await
        .expect("live read-only fork did not finish opening in time")
        .unwrap_or_else(|error| panic!("failed to open `{backend}` read-only fork: {error}"));
    let fork_token = format!("UMADEV_LIVE_FORK_{}", std::process::id());
    let fork_text = run_read_only_turn(critic.as_mut(), &fork_token).await;
    eprintln!("{backend}: fork turn accepted: {}", fork_text.trim());

    critic.end().await.expect("close live read-only fork");
    session.end().await.expect("close live parent session");

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("sentinel.txt"))
            .expect("read live-probe sentinel after turns"),
        "UmaDev live base acceptance sentinel\n"
    );
    assert_eq!(
        workspace_snapshot(workspace.path()),
        before,
        "Plan-profile parent or fork changed the isolated workspace"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "makes a real model call and executes tools in a tempdir; set UMADEV_LIVE_BASE"]
async fn installed_base_auto_profile_writes_and_binds_loopback() {
    let backend = selected_backend();
    let workspace = TempDir::new().expect("create isolated full-access workspace");
    let probe = workspace.path().join("port-probe");
    std::fs::create_dir_all(probe.join("src")).expect("create local-port probe source tree");
    std::fs::write(
        probe.join("Cargo.toml"),
        "[package]\nname = \"umadev-live-port-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write local-port probe manifest");
    let token = format!("UMADEV_FULL_ACCESS_{}", std::process::id());
    std::fs::write(
        probe.join("src/main.rs"),
        format!(
            r#"use std::io::{{Read, Write}};
use std::net::{{TcpListener, TcpStream}};

fn main() {{
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let address = listener.local_addr().expect("read loopback address");
    let client = std::thread::spawn(move || {{
        let mut stream = TcpStream::connect(address).expect("connect loopback");
        stream.write_all(b"umadev-port-probe").expect("write loopback");
    }});
    let (mut stream, _) = listener.accept().expect("accept loopback");
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).expect("read loopback");
    client.join().expect("join loopback client");
    assert_eq!(bytes, b"umadev-port-probe");
    std::fs::write("port-probe.ok", "{token}\n").expect("write port proof");
    println!("UMADEV_PORT_OK");
}}
"#
        ),
    )
    .expect("write local-port probe program");

    let mut session = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_with_policy(
            &backend,
            workspace.path(),
            "",
            BasePermissionProfile::Auto,
            Some("Operate only inside the supplied isolated acceptance workspace."),
            SessionOpenPolicy::NonInteractive,
        ),
    )
    .await
    .expect("full-access base did not finish opening in time")
    .unwrap_or_else(|error| panic!("failed to open Auto `{backend}` session: {error}"));

    run_full_access_turn(session.as_mut(), &token).await;
    session.end().await.expect("close live full-access session");

    assert_eq!(
        std::fs::read_to_string(workspace.path().join("write-probe.txt"))
            .expect("base did not create write-probe.txt"),
        format!("{token}\n")
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("port-probe.ok"))
            .expect("local-port program did not produce its proof"),
        format!("{token}\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "makes real model calls; set UMADEV_LIVE_BASE and run explicitly"]
async fn installed_base_resumes_native_context_without_writing() {
    let backend = selected_backend();
    let workspace = TempDir::new().expect("create isolated resume workspace");
    std::fs::write(workspace.path().join("sentinel.txt"), "resume sentinel\n")
        .expect("write resume sentinel");
    let before = workspace_snapshot(workspace.path());

    if backend == "grok-build" {
        let result = umadev_host::session_for_resume_with_policy(
            &backend,
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            None,
            "opaque-unattested-session-id",
            SessionOpenPolicy::NonInteractive,
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(mut session) => {
                let _ = session.end().await;
                panic!("unattested Grok resume must fail closed");
            }
        };
        let message = error.to_string();
        assert!(message.contains("effective sandbox attestation"));
        assert!(message.contains("native resume preflight"));
        assert_eq!(workspace_snapshot(workspace.path()), before);
        return;
    }

    let secret = format!("UMADEV_RESUME_SECRET_{}", std::process::id());
    let ack = format!("UMADEV_RESUME_STORED_{}", std::process::id());
    let mut first = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_with_policy(
            &backend,
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            None,
            SessionOpenPolicy::NonInteractive,
        ),
    )
    .await
    .expect("initial resume-probe session did not open in time")
    .unwrap_or_else(|error| panic!("failed to open initial `{backend}` session: {error}"));
    run_expected_read_only_turn(
        first.as_mut(),
        format!(
            "Remember this secret for the next turn: {secret}. Do not use tools or modify files. Reply with exactly {ack} and nothing else."
        ),
        &ack,
    )
    .await;
    let session_id = first
        .session_id()
        .map(str::to_string)
        .expect("live base did not expose its native session id");
    first.end().await.expect("close first resume process");
    drop(first);

    let mut resumed = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_resume_with_policy(
            &backend,
            workspace.path(),
            "",
            BasePermissionProfile::Plan,
            None,
            &session_id,
            SessionOpenPolicy::NonInteractive,
        ),
    )
    .await
    .expect("native resume did not finish opening in time")
    .unwrap_or_else(|error| panic!("failed to resume `{backend}` session: {error}"));
    run_expected_read_only_turn(
        resumed.as_mut(),
        "Reply with the exact secret I asked you to remember in the previous turn. Output only the secret and do not use tools.".to_string(),
        &secret,
    )
    .await;
    resumed.end().await.expect("close resumed process");

    assert_eq!(workspace_snapshot(workspace.path()), before);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("sentinel.txt"))
            .expect("read resume sentinel"),
        "resume sentinel\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "makes real Grok model calls and executes tools in a tempdir"]
async fn installed_grok_uses_its_server_authoritative_prompt_queue() {
    if selected_backend() != "grok-build" {
        eprintln!("skipped: this vendor-private acceptance check requires grok-build");
        return;
    }

    let workspace = TempDir::new().expect("create isolated Grok queue workspace");
    let probe = workspace.path().join("queue-delay");
    std::fs::create_dir_all(probe.join("src")).expect("create queue-delay source tree");
    std::fs::write(
        probe.join("Cargo.toml"),
        "[package]\nname = \"umadev-live-queue-delay\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write queue-delay manifest");
    std::fs::write(
        probe.join("src/main.rs"),
        "fn main() { std::thread::sleep(std::time::Duration::from_secs(4)); println!(\"UMADEV_QUEUE_DELAY_OK\"); }\n",
    )
    .expect("write queue-delay program");

    let mut session = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_with_policy(
            "grok-build",
            workspace.path(),
            "",
            BasePermissionProfile::Auto,
            Some("Operate only inside the supplied isolated acceptance workspace."),
            SessionOpenPolicy::NonInteractive,
        ),
    )
    .await
    .expect("Grok queue session did not open in time")
    .unwrap_or_else(|error| panic!("failed to open Grok queue session: {error}"));
    assert!(
        session
            .capabilities()
            .supports(SessionCapability::PromptQueue),
        "audited Grok 0.2.112 did not negotiate its native prompt queue"
    );
    let session_id = session
        .session_id()
        .expect("Grok queue session omitted its native id")
        .to_string();
    let first_token = format!("UMADEV_QUEUE_FIRST_{}", std::process::id());
    let second_token = format!("UMADEV_QUEUE_SECOND_{}", std::process::id());

    session
        .send_turn(format!(
            "Run exactly `cargo run --quiet --manifest-path queue-delay/Cargo.toml` with the normal foreground terminal tool. After it exits, reply with exactly `{first_token}` and nothing else."
        ))
        .await
        .expect("start first Grok queue turn");
    session
        .enqueue_input(
            TurnInput::text(format!(
                "Do not call tools. Reply with exactly `{second_token}` and nothing else."
            )),
            PromptQueuePlacement::Tail,
        )
        .await
        .expect("enqueue second Grok prompt through the native queue");

    let deadline = Instant::now() + TURN_TIMEOUT;
    let mut text = String::new();
    let mut completed_turns = 0_u8;
    let mut saw_queued = false;
    let mut saw_server_drain = false;
    let mut saw_delay_command = false;
    while completed_turns < 2 {
        let event = timeout_at(deadline, session.next_event())
            .await
            .expect("Grok native queue did not drain before its deadline")
            .expect("Grok queue session ended before both turns completed");
        match event {
            SessionEvent::TextDelta(delta) => text.push_str(&delta),
            SessionEvent::ToolCall { input, .. }
            | SessionEvent::ToolCallCorrelated { input, .. } => {
                saw_delay_command |= input
                    .to_string()
                    .contains("cargo run --quiet --manifest-path queue-delay/Cargo.toml");
            }
            SessionEvent::PromptQueueChanged(snapshot) => {
                assert_eq!(snapshot.session_id, session_id);
                if snapshot
                    .entries
                    .iter()
                    .any(|entry| entry.text.contains(&second_token))
                {
                    saw_queued = true;
                }
                if saw_queued && snapshot.entries.is_empty() {
                    saw_server_drain = true;
                }
            }
            SessionEvent::NeedApproval { action, target, .. } => {
                panic!(
                    "Auto queue acceptance unexpectedly requested approval for {action}: {target}"
                );
            }
            SessionEvent::HostRequest { request, .. } => {
                panic!("Auto queue acceptance unexpectedly requested host input: {request:?}");
            }
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            } => completed_turns += 1,
            SessionEvent::TurnDone { status, .. } => {
                panic!("Grok queued turn did not complete cleanly: {status:?}");
            }
            _ => {}
        }
    }
    session.end().await.expect("close Grok queue session");

    assert!(
        saw_delay_command,
        "first queued turn skipped its real delay command"
    );
    assert!(
        saw_queued,
        "no authoritative snapshot exposed the queued prompt"
    );
    assert!(
        saw_server_drain,
        "no authoritative snapshot removed the prompt when it began draining"
    );
    assert!(
        text.contains(&first_token),
        "first queued response was missing: {text:?}"
    );
    assert!(
        text.contains(&second_token),
        "second queued response was missing: {text:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "makes a real Grok model call and starts then stops a temp background process"]
async fn installed_grok_lists_and_stops_its_owned_background_process() {
    if selected_backend() != "grok-build" {
        eprintln!("skipped: this vendor-private acceptance check requires grok-build");
        return;
    }

    let workspace = TempDir::new().expect("create isolated Grok background workspace");
    let probe = workspace.path().join("background-probe");
    std::fs::create_dir_all(probe.join("src")).expect("create background-probe source tree");
    std::fs::write(
        probe.join("Cargo.toml"),
        "[package]\nname = \"umadev-live-background-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("write background-probe manifest");
    std::fs::write(
        probe.join("src/main.rs"),
        r#"fn main() {
    std::fs::write("bg.started", "started\n").expect("write start proof");
    std::thread::sleep(std::time::Duration::from_secs(30));
    std::fs::write("bg.finished", "finished\n").expect("write finish proof");
}
"#,
    )
    .expect("write background-probe program");

    let mut session = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_with_policy(
            "grok-build",
            workspace.path(),
            "",
            BasePermissionProfile::Auto,
            Some("Operate only inside the supplied isolated acceptance workspace."),
            SessionOpenPolicy::NonInteractive,
        ),
    )
    .await
    .expect("Grok background session did not open in time")
    .unwrap_or_else(|error| panic!("failed to open Grok background session: {error}"));
    assert!(
        session
            .capabilities()
            .supports(SessionCapability::BackgroundProcessControl),
        "audited Grok 0.2.112 did not negotiate native background control"
    );
    let token = format!("UMADEV_BACKGROUND_STARTED_{}", std::process::id());
    session
        .send_turn(format!(
            "Use the native terminal execution tool once to run exactly `cargo run --quiet --manifest-path background-probe/Cargo.toml` with its `is_background` parameter set to true. Do not wait for it and do not stop it. After the tool returns its task id, reply with exactly `{token}` and nothing else."
        ))
        .await
        .expect("start Grok background-process turn");

    let deadline = Instant::now() + TURN_TIMEOUT;
    let mut text = String::new();
    let mut started_task_id = None;
    let mut saw_background_argument = false;
    let mut tool_evidence = Vec::new();
    loop {
        let event = timeout_at(deadline, session.next_event())
            .await
            .expect("Grok background turn exceeded its deadline")
            .expect("Grok background session ended before TurnDone");
        match event {
            SessionEvent::TextDelta(delta) => text.push_str(&delta),
            SessionEvent::ToolCall { name, input }
            | SessionEvent::ToolCallCorrelated { name, input, .. } => {
                let command = input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let background = input
                    .get("is_background")
                    .or_else(|| input.get("background"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                saw_background_argument |=
                    command.contains("background-probe/Cargo.toml") && background;
                tool_evidence.push(format!("{name}: {input}"));
            }
            SessionEvent::BackgroundProcess(BackgroundProcessSignal::Started { process }) => {
                started_task_id = Some(process.task_id);
            }
            SessionEvent::NeedApproval { action, target, .. } => {
                panic!("Auto background acceptance requested approval for {action}: {target}");
            }
            SessionEvent::HostRequest { request, .. } => {
                panic!("Auto background acceptance requested host input: {request:?}");
            }
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                ..
            } => break,
            SessionEvent::TurnDone { status, .. } => {
                panic!("Grok background turn did not complete cleanly: {status:?}");
            }
            _ => {}
        }
    }
    let snapshot = session
        .list_background_processes()
        .await
        .expect("list Grok background processes through x.ai/task/list");
    let task_id = started_task_id
        .or_else(|| {
            snapshot
                .processes
                .iter()
                .find(|process| !process.completed)
                .map(|process| process.task_id.clone())
        })
        .expect("Grok exposed neither a background lifecycle edge nor a live owned task");
    let listed_live_owned_task = snapshot
        .processes
        .iter()
        .any(|process| process.task_id == task_id && !process.completed);

    let start_deadline = Instant::now() + Duration::from_secs(60);
    let mut reached_start_proof = false;
    while !workspace.path().join("bg.started").exists() {
        if Instant::now() >= start_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if workspace.path().join("bg.started").exists() {
        reached_start_proof = true;
    }
    let outcome = session
        .stop_background_process(&task_id)
        .await
        .expect("stop the exact owned Grok background process");
    assert_eq!(outcome, BackgroundProcessStopOutcome::Killed);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !workspace.path().join("bg.finished").exists(),
        "the native stop returned Killed but the background program reached normal completion"
    );
    session.end().await.expect("close Grok background session");

    assert!(
        text.contains(&token),
        "background response was missing: {text:?}"
    );
    assert!(
        saw_background_argument,
        "Grok did not expose its native background terminal argument: {tool_evidence:?}"
    );
    assert!(
        listed_live_owned_task,
        "native list did not return the live task owned by this session: {snapshot:?}"
    );
    assert!(
        reached_start_proof,
        "background program never reached its start proof"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opens an authenticated interactive Grok session in an isolated untrusted folder"]
async fn installed_grok_folder_trust_rejection_keeps_project_config_gated() {
    if selected_backend() != "grok-build" {
        eprintln!("skipped: this vendor-private acceptance check requires grok-build");
        return;
    }
    if std::env::var_os("GROK_TEST_VERSION").is_none()
        || std::env::var("GROK_FOLDER_TRUST").as_deref() != Ok("1")
    {
        eprintln!(
            "skipped: set GROK_TEST_VERSION and GROK_FOLDER_TRUST=1 to exercise the official release-stamped Folder Trust path"
        );
        return;
    }

    let workspace = TempDir::new().expect("create isolated Grok Folder Trust workspace");
    std::fs::write(
        workspace.path().join(".envrc"),
        "printf 'executed\\n' > folder-trust-executed.txt\n",
    )
    .expect("write inert Folder Trust marker config");
    let original_envrc = std::fs::read(workspace.path().join(".envrc")).unwrap();

    let mut session = timeout(
        OPEN_TIMEOUT,
        umadev_host::session_for_with_policy_and_surface(
            "grok-build",
            workspace.path(),
            "",
            BasePermissionProfile::Auto,
            Some("Operate only inside the supplied isolated acceptance workspace."),
            SessionOpenPolicy::NonInteractive,
            FolderTrustClientSurface::Interactive,
        ),
    )
    .await
    .expect("interactive Grok Folder Trust session did not open in time")
    .unwrap_or_else(|error| panic!("failed to open interactive Grok session: {error}"));

    let trust_deadline = Instant::now() + Duration::from_secs(15);
    let (req_id, config_kinds) = loop {
        let event = timeout_at(trust_deadline, session.next_event())
            .await
            .expect("Grok did not issue its Folder Trust request")
            .expect("Grok session ended before its Folder Trust request");
        match event {
            SessionEvent::HostRequest {
                req_id,
                request:
                    HostRequest::FolderTrust {
                        cwd, config_kinds, ..
                    },
            } => {
                assert_eq!(cwd, workspace.path());
                break (req_id, config_kinds);
            }
            SessionEvent::HostRequest { request, .. } => {
                panic!("unexpected host request before Folder Trust: {request:?}");
            }
            _ => {}
        }
    };
    assert!(
        config_kinds.iter().any(|kind| kind == "envrc"),
        "Grok did not identify the repo-local envrc gate: {config_kinds:?}"
    );
    session
        .respond_host(
            &req_id,
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::KeepGated,
            },
        )
        .await
        .expect("settle Grok Folder Trust as KeepGated");

    // The gate is evaluated during session creation. Give Grok's detached
    // response task time to consume KeepGated; a model turn is neither needed
    // nor allowed to become a substitute for the trust-boundary proof.
    tokio::time::sleep(Duration::from_millis(500)).await;
    session
        .end()
        .await
        .expect("close Grok Folder Trust session");

    assert_eq!(
        std::fs::read(workspace.path().join(".envrc")).unwrap(),
        original_envrc,
        "Folder Trust flow modified the project config"
    );
    assert!(
        !workspace.path().join("folder-trust-executed.txt").exists(),
        "rejected project config executed despite KeepGated"
    );
}
