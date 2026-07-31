use super::*;

use std::io::{BufRead as _, Write as _};

fn invoked(name: &str) -> bool {
    std::env::args().any(|arg| arg == "--exact") && std::env::args().any(|arg| arg.ends_with(name))
}

fn emit(stdout: &mut std::io::Stdout, value: impl Into<Value>) {
    let value = value.into();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn initialize_result(frame: &Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":frame["id"],
        "result":{
            "protocolVersion":1,
            "agentCapabilities":{"loadSession":true},
            "_meta":{
                "grokShell":true,
                "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
            }
        }
    })
}

fn trust_request(id: u64, session_id: &str, cwd: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"_x.ai/folder_trust/request",
        "params":{
            "sessionId":session_id,
            "cwd":cwd,
            "workspace":cwd,
            "configKinds":["project rules","MCP configuration"]
        }
    })
}

fn read_frame(lines: &mut impl Iterator<Item = std::io::Result<String>>) -> Value {
    serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap()
}

#[test]
fn fake_grok_folder_trust_headless_child() {
    if !invoked("fake_grok_folder_trust_headless_child") {
        return;
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();
    let mut cwd = None;
    loop {
        let frame = read_frame(&mut lines);
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                assert!(frame
                    .pointer("/params/clientCapabilities/_meta/x.ai~1folderTrust")
                    .is_none());
                emit(&mut stdout, initialize_result(&frame));
            }
            Some("session/new") => {
                cwd = frame["params"]["cwd"].as_str().map(str::to_string);
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"sessionId":"trust-session"}}),
                );
            }
            Some("session/set_mode") => {
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{}}),
                );
                emit(
                    &mut stdout,
                    trust_request(910, "trust-session", cwd.as_deref().unwrap()),
                );
                let response = read_frame(&mut lines);
                assert_eq!(response["id"], 910);
                assert_eq!(response["result"], json!({"outcome":"reject"}));
            }
            Some("_x.ai/session/close") => {
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"result":{"success":true}}}),
                );
                return;
            }
            other => panic!("unexpected headless Folder Trust frame {other:?}"),
        }
    }
}

#[test]
fn fake_grok_folder_trust_interactive_child() {
    if !invoked("fake_grok_folder_trust_interactive_child") {
        return;
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();
    let mut cwd = None;
    loop {
        let frame = read_frame(&mut lines);
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                assert_eq!(
                    frame.pointer("/params/clientCapabilities/_meta/x.ai~1folderTrust/interactive"),
                    Some(&Value::Bool(true))
                );
                emit(&mut stdout, initialize_result(&frame));
            }
            Some("session/new") => {
                cwd = frame["params"]["cwd"].as_str().map(str::to_string);
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"sessionId":"trust-session"}}),
                );
            }
            Some("session/set_mode") => {
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{}}),
                );
                let cwd = cwd.as_deref().unwrap();

                emit(&mut stdout, trust_request(920, "foreign-session", cwd));
                let response = read_frame(&mut lines);
                assert_eq!(response["id"], 920);
                assert_ne!(response.pointer("/result/outcome"), Some(&json!("trust")));

                let invalid_cwd = format!("{cwd}/..");
                emit(
                    &mut stdout,
                    trust_request(921, "trust-session", &invalid_cwd),
                );
                let response = read_frame(&mut lines);
                assert_eq!(response["id"], 921);
                assert_eq!(response["result"], json!({"outcome":"reject"}));

                emit(&mut stdout, trust_request(922, "trust-session", cwd));
                let response = read_frame(&mut lines);
                assert_eq!(response["id"], 922);
                assert_eq!(response["result"], json!({"outcome":"trust"}));

                emit(&mut stdout, trust_request(923, "trust-session", cwd));
                let response = read_frame(&mut lines);
                assert_eq!(response["id"], 923);
                assert_eq!(response["result"], json!({"outcome":"reject"}));
            }
            Some("_x.ai/session/close") => {
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"result":{"success":true}}}),
                );
                return;
            }
            other => panic!("unexpected interactive Folder Trust frame {other:?}"),
        }
    }
}

#[test]
fn fake_grok_folder_trust_precedes_new_session_response_child() {
    if !invoked("fake_grok_folder_trust_precedes_new_session_response_child") {
        return;
    }
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();
    let mut trust_settled = false;
    loop {
        let frame = read_frame(&mut lines);
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                assert_eq!(
                    frame.pointer("/params/clientCapabilities/_meta/x.ai~1folderTrust/interactive"),
                    Some(&Value::Bool(true))
                );
                emit(&mut stdout, initialize_result(&frame));
            }
            Some("session/new") => {
                let cwd = frame["params"]["cwd"].as_str().unwrap();
                emit(&mut stdout, trust_request(930, "early-session", cwd));
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"sessionId":"early-session"}}),
                );
            }
            Some("session/set_mode") => {
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{}}),
                );
            }
            Some("_x.ai/session/close") => {
                assert!(
                    trust_settled,
                    "early Folder Trust request was never settled"
                );
                emit(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":{"result":{"success":true}}}),
                );
                return;
            }
            None if frame.get("id") == Some(&json!(930)) => {
                assert_eq!(frame["result"], json!({"outcome":"reject"}));
                trust_settled = true;
            }
            other => panic!("unexpected early Folder Trust frame {other:?}: {frame}"),
        }
    }
}

fn child_args(test_name: &str) -> Vec<String> {
    vec![
        "--exact".to_string(),
        format!("acp::folder_trust_tests::{test_name}"),
        "--nocapture".to_string(),
        "--test-threads=1".to_string(),
    ]
}

#[tokio::test]
async fn headless_open_never_advertises_or_surfaces_folder_trust() {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut session = AcpSession::start_with_program_args(
        AcpVendor::Grok,
        executable.to_str().unwrap(),
        child_args("fake_grok_folder_trust_headless_child"),
        workspace.path(),
        "",
        BasePermissionProfile::Auto,
        None,
    )
    .await
    .unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(150), session.next_event())
            .await
            .is_err()
    );
    session.end().await.unwrap();
}

#[tokio::test]
async fn interactive_auto_profile_still_requires_exact_human_folder_trust_decisions() {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut session = AcpSession::start_with_program_args_and_firmware_and_policy(
        AcpVendor::Grok,
        executable.to_str().unwrap(),
        child_args("fake_grok_folder_trust_interactive_child"),
        workspace.path(),
        "",
        BasePermissionProfile::Auto,
        None,
        None,
        SessionOpenPolicy::NonInteractive,
        FolderTrustClientSurface::Interactive,
        Some(audited_grok_test_capabilities()),
    )
    .await
    .unwrap();

    let (req_id, cwd) = match tokio::time::timeout(Duration::from_secs(5), session.next_event())
        .await
        .unwrap()
        .unwrap()
    {
        SessionEvent::HostRequest {
            req_id,
            request:
                HostRequest::FolderTrust {
                    cwd,
                    workspace: trust_key,
                    config_kinds,
                },
        } => {
            assert_eq!(cwd, trust_key);
            assert_eq!(config_kinds, ["project rules", "MCP configuration"]);
            (req_id, cwd)
        }
        other => panic!("expected explicit Folder Trust request, got {other:?}"),
    };
    session
        .respond_host(
            &req_id,
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::Trust,
            },
        )
        .await
        .unwrap();

    let req_id = match tokio::time::timeout(Duration::from_secs(5), session.next_event())
        .await
        .unwrap()
        .unwrap()
    {
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::FolderTrust { cwd: next_cwd, .. },
        } => {
            assert_eq!(next_cwd, cwd);
            req_id
        }
        other => panic!("expected second Folder Trust request, got {other:?}"),
    };
    session
        .respond_host(
            &req_id,
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::KeepGated,
            },
        )
        .await
        .unwrap();
    session.end().await.unwrap();
}

#[tokio::test]
async fn interactive_folder_trust_is_buffered_until_session_new_binds_its_identity() {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut session = AcpSession::start_with_program_args_and_firmware_and_policy(
        AcpVendor::Grok,
        executable.to_str().unwrap(),
        child_args("fake_grok_folder_trust_precedes_new_session_response_child"),
        workspace.path(),
        "",
        BasePermissionProfile::Auto,
        None,
        None,
        SessionOpenPolicy::NonInteractive,
        FolderTrustClientSurface::Interactive,
        Some(audited_grok_test_capabilities()),
    )
    .await
    .unwrap();

    let req_id = match tokio::time::timeout(Duration::from_secs(5), session.next_event())
        .await
        .unwrap()
        .unwrap()
    {
        SessionEvent::HostRequest {
            req_id,
            request: HostRequest::FolderTrust { cwd, .. },
        } => {
            assert_eq!(cwd, workspace.path());
            req_id
        }
        other => panic!("expected buffered Folder Trust request, got {other:?}"),
    };
    session
        .respond_host(
            &req_id,
            HostResponse::FolderTrust {
                decision: HostFolderTrustDecision::KeepGated,
            },
        )
        .await
        .unwrap();
    session.end().await.unwrap();
}
