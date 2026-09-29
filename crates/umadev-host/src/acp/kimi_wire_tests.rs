//! Kimi Code 0.31's exact wire shapes, replayed by a scripted child process.

use super::*;

use std::io::{BufRead as _, Write as _};

const SESSION: &str = "kimi-fixture-session";

fn invoked(name: &str) -> bool {
    std::env::args().any(|arg| arg == "--exact") && std::env::args().any(|arg| arg.ends_with(name))
}

fn emit(stdout: &mut std::io::Stdout, value: &Value) {
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn session_update(update: Value) -> Value {
    json!({
        "jsonrpc":"2.0", "method":"session/update",
        "params":{"sessionId":SESSION, "update":update}
    })
}

/// `toolCallStartToSessionUpdate`: the call's kind and parsed arguments.
fn tool_call(call_id: &str, title: &str, kind: &str, raw_input: &Value) -> Value {
    session_update(json!({
        "sessionUpdate":"tool_call", "toolCallId":call_id, "title":title, "kind":kind,
        "status":"in_progress", "rawInput":raw_input,
        "content":[{"type":"content","content":{"type":"text","text":raw_input.to_string()}}]
    }))
}

/// `buildPermissionToolCallUpdate`: only the call id, the tool name as
/// `title`, and a prose summary. No `kind`, no `rawInput`.
fn permission_request(id: &str, call_id: &str, tool: &str, action: &str) -> Value {
    json!({
        "jsonrpc":"2.0", "id":id, "method":"session/request_permission",
        "params":{
            "sessionId":SESSION,
            "toolCall":{
                "toolCallId":call_id, "title":tool,
                "content":[{"type":"content","content":{
                    "type":"text","text":format!("Requesting approval to {action}")
                }}]
            },
            "options":[
                {"optionId":"approve_once","name":"Approve once","kind":"allow_once"},
                {"optionId":"approve_always","name":"Approve for this session","kind":"allow_always"},
                {"optionId":"reject","name":"Reject","kind":"reject_once"}
            ]
        }
    })
}

#[derive(Default)]
struct ScriptedKimi {
    prompt_id: Option<Value>,
}

impl ScriptedKimi {
    fn handle(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => emit(
                stdout,
                &json!({
                    "jsonrpc":"2.0", "id":frame["id"], "result":{
                        "protocolVersion":1,
                        "agentInfo":{
                            "name":"Kimi Code CLI",
                            "version":crate::kimi_contract::KIMI_CODE_AUDITED_BASELINE_VERSION
                        },
                        "agentCapabilities":{"loadSession":true},
                        "authMethods":[{"id":"login","type":"terminal","args":["--login"]}]
                    }
                }),
            ),
            Some("authenticate") => emit(stdout, &json!({"jsonrpc":"2.0","id":frame["id"],"result":{}})),
            Some("session/new") => emit(
                stdout,
                &json!({
                    "jsonrpc":"2.0", "id":frame["id"], "result":{
                        "sessionId":SESSION,
                        "configOptions":super::tests::kimi_fixture_config_options("model-a", "default")
                    }
                }),
            ),
            Some("session/set_config_option") => {
                let mode = frame["params"]["value"].as_str().unwrap_or("default");
                emit(
                    stdout,
                    &json!({
                        "jsonrpc":"2.0", "id":frame["id"], "result":{
                            "configOptions":super::tests::kimi_fixture_config_options("model-a", mode)
                        }
                    }),
                );
            }
            Some("session/prompt") => {
                self.prompt_id = frame.get("id").cloned();
                let text = frame["params"]["prompt"][0]["text"].as_str().unwrap_or("");
                self.run_prompt(text, stdout);
            }
            None if frame.get("result").is_some() => {
                // UmaDev answered a permission request: record exactly what it
                // selected, then end the turn the way Kimi does after a decision.
                let id = frame["id"].as_str().unwrap_or("unknown");
                std::fs::write(format!("reply-{id}.json"), frame["result"].to_string()).unwrap();
                self.finish_prompt(stdout);
            }
            _ => {}
        }
    }

    fn run_prompt(&mut self, text: &str, stdout: &mut std::io::Stdout) {
        match text {
            "classify shell" => {
                let input = json!({"command":"rm -rf build"});
                emit(stdout, &tool_call("1:c1", "Running: rm -rf build", "execute", &input));
                emit(
                    stdout,
                    &permission_request("perm-shell", "1:c1", "Bash", "Running: rm -rf build"),
                );
            }
            "classify write" => {
                // Kimi's Write display is `file_io` without before/after, so the
                // request carries no diff card: the path exists only on the call.
                let input = json!({"path":"/home/dev/.zshrc","content":"export EDITOR=vim\n"});
                emit(stdout, &tool_call("1:c2", "Writing /home/dev/.zshrc", "edit", &input));
                emit(
                    stdout,
                    &permission_request("perm-write", "1:c2", "Write", "Writing /home/dev/.zshrc"),
                );
            }
            other => panic!("scripted Kimi received an unexpected prompt {other:?}"),
        }
    }

    fn finish_prompt(&mut self, stdout: &mut std::io::Stdout) {
        if let Some(id) = self.prompt_id.take() {
            emit(
                stdout,
                &json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
            );
        }
    }
}

#[test]
fn fake_kimi_scripted_child() {
    if !invoked("fake_kimi_scripted_child") {
        return;
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();
    let mut kimi = ScriptedKimi::default();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        kimi.handle(&frame, &mut stdout);
    }
}

async fn start_scripted_kimi(workspace: &Path, permissions: BasePermissionProfile) -> AcpSession {
    let executable = std::env::current_exe().unwrap();
    AcpSession::start_with_program_args(
        AcpVendor::Kimi,
        executable.to_str().unwrap(),
        vec![
            "--exact".to_string(),
            "acp::kimi_wire_tests::fake_kimi_scripted_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ],
        workspace,
        "",
        permissions,
        None,
    )
    .await
    .unwrap()
}

/// The next approval the session surfaces, as `(req_id, action, target)`.
async fn next_approval(session: &mut AcpSession) -> (String, String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, session.next_event())
            .await
            .expect("no approval surfaced within 2 s")
            .expect("the Kimi fixture session closed");
        if let SessionEvent::HostRequest {
            req_id,
            request: HostRequest::Approval { action, target, .. },
        } = event
        {
            return (req_id, action, target);
        }
    }
}

async fn next_turn_status(session: &mut AcpSession) -> TurnStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let event = tokio::time::timeout_at(deadline, session.next_event())
            .await
            .expect("the turn did not finish within 2 s")
            .expect("the Kimi fixture session closed");
        if let SessionEvent::TurnDone { status, .. } = event {
            return status;
        }
    }
}

async fn deny(session: &mut AcpSession, req_id: &str) {
    session
        .respond_host(
            req_id,
            HostResponse::Approval {
                decision: ApprovalDecision::Deny,
                selected_option_id: Some("reject".to_string()),
                message: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn kimi_permission_request_is_classified_by_the_correlated_tool_call() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = start_scripted_kimi(workspace.path(), BasePermissionProfile::Guarded).await;
    for (prompt, expected_action, expected_target) in [
        // The trust floor must see the command, not the tool name `Bash`.
        ("classify shell", "Bash", "rm -rf build"),
        // A write is judged by its path (here outside any workspace), never by
        // the tool name `Write`, which reads as an in-tree relative path.
        ("classify write", "Edit", "/home/dev/.zshrc"),
    ] {
        session.send_turn(prompt.to_string()).await.unwrap();
        let (req_id, action, target) = next_approval(&mut session).await;
        assert_eq!(
            (action.as_str(), target.as_str()),
            (expected_action, expected_target),
            "{prompt}"
        );
        deny(&mut session, &req_id).await;
        assert_eq!(next_turn_status(&mut session).await, TurnStatus::Completed);
    }
    session.end().await.unwrap();
}

#[test]
fn approval_subject_uses_the_announced_call_then_the_diff_path_then_the_text() {
    let mut tools = ToolState::default();
    let text = |text: &str| json!([{"type":"content","content":{"type":"text","text":text}}]);
    // Kimi's lazy create has no arguments yet; its started upgrade adds them.
    parse_session_update(
        &session_update(json!({
            "sessionUpdate":"tool_call", "toolCallId":"2:w1", "title":"Write",
            "kind":"edit", "status":"pending", "content":text("{\"pa")
        }))["params"],
        &mut tools,
    );
    parse_session_update(
        &session_update(json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"2:w1",
            "title":"Writing /home/dev/.zshrc", "kind":"edit", "status":"in_progress",
            "rawInput":{"path":"/home/dev/.zshrc","content":"x"}
        }))["params"],
        &mut tools,
    );
    let request = json!({
        "toolCallId":"2:w1", "title":"Write",
        "content":text("Requesting approval to Writing /home/dev/.zshrc")
    });
    assert_eq!(
        approval_subject(&request, &tools),
        ("Edit".to_string(), "/home/dev/.zshrc".to_string())
    );

    // A request that carries its own kind and arguments is judged by them.
    let own = json!({
        "toolCallId":"2:w1", "title":"Run", "kind":"execute",
        "rawInput":{"command":"cargo test"}
    });
    assert_eq!(
        approval_subject(&own, &tools),
        ("Bash".to_string(), "cargo test".to_string())
    );

    // A settled call is forgotten; what is left is the request's own text.
    parse_session_update(
        &session_update(json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"2:w1",
            "status":"completed", "rawOutput":"ok"
        }))["params"],
        &mut tools,
    );
    assert_eq!(
        approval_subject(&request, &tools).1,
        "Requesting approval to Writing /home/dev/.zshrc"
    );

    // An unannounced edit is judged by its diff card's path.
    let edit = json!({
        "toolCallId":"2:e9", "title":"Edit",
        "content":[
            {"type":"diff","path":"/work/app/src/lib.rs","oldText":"a","newText":"b"},
            {"type":"content","content":{"type":"text","text":"Requesting approval to Editing src/lib.rs"}}
        ]
    });
    assert_eq!(
        approval_subject(&edit, &tools),
        ("Edit".to_string(), "/work/app/src/lib.rs".to_string())
    );

    // Never the bare tool name: `Write` is no path.
    assert_eq!(
        approval_subject(&json!({"toolCallId":"2:x", "title":"Write"}), &tools),
        ("Write".to_string(), String::new())
    );
}
