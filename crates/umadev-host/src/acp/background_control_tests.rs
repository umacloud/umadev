use super::*;

use std::io::{BufRead as _, Write as _};

fn emit(stdout: &mut std::io::Stdout, value: &Value) {
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn task(task_id: &str, owner: Option<&str>, completed: bool) -> Value {
    json!({
        "task_id":task_id,
        "command":"never expose this command",
        "display_command":Value::Null,
        "cwd":"/never/expose/cwd",
        "start_time":{"secs_since_epoch":1,"nanos_since_epoch":0},
        "end_time":if completed { json!({"secs_since_epoch":2,"nanos_since_epoch":0}) } else { Value::Null },
        "output":"never expose this output",
        "output_file":"/never/expose/output.log",
        "truncated":false,
        "exit_code":if completed { json!(0) } else { Value::Null },
        "signal":Value::Null,
        "completed":completed,
        "kind":"bash",
        "block_waited":false,
        "explicitly_killed":completed,
        "owner_session_id":owner,
    })
}

fn lifecycle_started() -> Value {
    json!({
        "jsonrpc":"2.0",
        "method":"_x.ai/task_backgrounded",
        "params":{
            "sessionId":"control-session",
            "update":{
                "sessionUpdate":"task_backgrounded",
                "tool_call_id":"tool-own-run",
                "task_id":"own-run",
                "command":"never expose this command",
                "cwd":"/never/expose/cwd",
                "output_file":"/never/expose/output.log",
                "description":"dev server"
            },
            "_meta":{"eventId":"start-own-run"}
        }
    })
}

fn lifecycle_finished(event_id: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "method":"_x.ai/task_completed",
        "params":{
            "sessionId":"control-session",
            "update":{
                "sessionUpdate":"task_completed",
                "task_snapshot":task("own-run", Some("control-session"), true),
                "will_wake":false
            },
            "_meta":{"eventId":event_id}
        }
    })
}

#[test]
fn fake_grok_background_control_child() {
    let invoked = std::env::args().any(|arg| arg == "--exact")
        && std::env::args().any(|arg| {
            arg.ends_with("acp::background_control_tests::fake_grok_background_control_child")
        });
    if !invoked {
        return;
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();

    let mut killed = false;
    let mut kill_calls = 0_u8;
    let mut started_emitted = false;
    for line in stdin.lock().lines().map_while(Result::ok) {
        let frame: Value = serde_json::from_str(&line).unwrap();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => emit(
                &mut stdout,
                &json!({
                    "jsonrpc":"2.0","id":frame["id"],
                    "result":{
                        "protocolVersion":1,
                        "agentCapabilities":{"loadSession":true},
                        "_meta":{
                            "grokShell":true,
                            "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                        }
                    }
                }),
            ),
            Some("session/new") => {
                emit(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0","id":frame["id"],
                        "result":{"sessionId":"control-session"}
                    }),
                );
            }
            Some("session/set_mode") => emit(
                &mut stdout,
                &json!({"jsonrpc":"2.0","id":frame["id"],"result":{}}),
            ),
            Some(GROK_TASK_LIST_METHOD) => {
                assert_eq!(frame["params"], json!({"sessionId":"control-session"}));
                if !started_emitted {
                    emit(&mut stdout, &lifecycle_started());
                    started_emitted = true;
                }
                let tasks = vec![
                    task("own-run", Some("control-session"), killed),
                    task("own-done", Some("control-session"), true),
                    task("foreign-run", Some("sibling-session"), false),
                    task("ownerless", None, false),
                ];
                emit(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0","id":frame["id"],
                        "result":{"result":{"tasks":tasks}}
                    }),
                );
            }
            Some(GROK_TASK_KILL_METHOD) => {
                kill_calls += 1;
                assert_eq!(
                    kill_calls, 1,
                    "an idempotent repeat must not send kill again"
                );
                assert_eq!(
                    frame["params"],
                    json!({"sessionId":"control-session","taskId":"own-run"})
                );
                killed = true;
                emit(
                    &mut stdout,
                    &json!({
                        "jsonrpc":"2.0","id":frame["id"],
                        "result":{"result":{"taskId":"own-run","outcome":"killed"}}
                    }),
                );
                emit(&mut stdout, &lifecycle_finished("finish-own-run-1"));
                emit(&mut stdout, &lifecycle_finished("finish-own-run-2"));
            }
            Some("_x.ai/session/close") => emit(
                &mut stdout,
                &json!({
                    "jsonrpc":"2.0","id":frame["id"],
                    "result":{"result":{"success":true}}
                }),
            ),
            _ => {}
        }
    }
}

async fn next_with_timeout(session: &mut AcpSession) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(5), session.next_event())
        .await
        .expect("background-control event timed out")
        .expect("background-control session closed")
}

#[tokio::test]
async fn grok_background_control_is_owned_authoritative_and_idempotent() {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let args = vec![
        "--exact".to_string(),
        "acp::background_control_tests::fake_grok_background_control_child".to_string(),
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
        .supports(SessionCapability::BackgroundProcessControl));

    let snapshot = session.list_background_processes().await.unwrap();
    assert_eq!(snapshot.session_id, "control-session");
    assert_eq!(
        snapshot
            .processes
            .iter()
            .map(|process| process.task_id.as_str())
            .collect::<Vec<_>>(),
        ["own-done", "own-run"]
    );
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("foreign-run"));
    assert!(!encoded.contains("ownerless"));
    assert!(!encoded.contains("never expose"));
    assert!(matches!(
        next_with_timeout(&mut session).await,
        SessionEvent::BackgroundProcess(BackgroundProcessSignal::Started {
            process: BackgroundProcessInfo { ref task_id, ref tool_call_id, .. }
        }) if task_id == "own-run" && tool_call_id == "tool-own-run"
    ));

    assert_eq!(
        session
            .stop_background_process("foreign-run")
            .await
            .unwrap(),
        BackgroundProcessStopOutcome::NotFound
    );
    assert_eq!(
        session.stop_background_process("own-done").await.unwrap(),
        BackgroundProcessStopOutcome::AlreadyExited
    );
    assert_eq!(
        session.stop_background_process("own-run").await.unwrap(),
        BackgroundProcessStopOutcome::Killed
    );
    assert_eq!(
        session.stop_background_process("own-run").await.unwrap(),
        BackgroundProcessStopOutcome::AlreadyExited
    );

    let mut finished = 0;
    while let Ok(Some(event)) =
        tokio::time::timeout(Duration::from_millis(150), session.next_event()).await
    {
        if matches!(
            event,
            SessionEvent::BackgroundProcess(BackgroundProcessSignal::Finished {
                ref task_id,
                ..
            }) if task_id == "own-run"
        ) {
            finished += 1;
        }
    }
    assert_eq!(
        finished, 1,
        "duplicate terminal notifications must settle once"
    );
    session.end().await.unwrap();
}

#[test]
fn unverified_future_grok_does_not_advertise_background_control() {
    let capabilities = negotiated_capabilities(
        AcpVendor::Grok,
        &json!({
            "protocolVersion":1,
            "agentCapabilities":{"loadSession":true},
            "_meta":{"grokShell":true,"agentVersion":"99.7.3-future.5+adapter"}
        }),
    );
    assert!(capabilities.grok_identity);
    assert!(capabilities.load, "standard ACP remains available");
    assert!(!capabilities.background_process_control);
}
