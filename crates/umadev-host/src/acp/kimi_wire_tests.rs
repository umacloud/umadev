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

fn session_update(update: &Value) -> Value {
    json!({
        "jsonrpc":"2.0", "method":"session/update",
        "params":{"sessionId":SESSION, "update":update}
    })
}

fn text_content(text: &str) -> Value {
    json!([{"type":"content","content":{"type":"text","text":text}}])
}

/// The frames Kimi sends for a call before it asks to run it.
///
/// `toolCallLazyCreateToSessionUpdate` opens a pending card from the first
/// streamed argument fragment, and every `toolCallDeltaToSessionUpdate`
/// replaces its text with all arguments streamed so far. Kimi resolves the
/// approval before it publishes `tool.call.started`, so the started upgrade
/// that carries `rawInput` only follows the answer.
fn streamed_call(call_id: &str, tool: &str, kind: &str, arguments: &Value) -> [Value; 2] {
    let arguments = arguments.to_string();
    let first = arguments.chars().take(12).collect::<String>();
    [
        session_update(&json!({
            "sessionUpdate":"tool_call", "toolCallId":call_id, "title":tool, "kind":kind,
            "status":"pending", "content":text_content(&first)
        })),
        session_update(&json!({
            "sessionUpdate":"tool_call_update", "toolCallId":call_id,
            "status":"in_progress", "content":text_content(&arguments)
        })),
    ]
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
                "content":text_content(&format!("Requesting approval to {action}"))
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
            Some("authenticate") => emit(
                stdout,
                &json!({"jsonrpc":"2.0","id":frame["id"],"result":{}}),
            ),
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
            Some("session/prompt") => self.run_prompt(frame, stdout),
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

    fn run_prompt(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
        self.prompt_id = frame.get("id").cloned();
        match frame["params"]["prompt"][0]["text"].as_str().unwrap_or("") {
            "classify shell" => {
                let arguments = json!({"command":"rm -rf build"});
                for frame in streamed_call("1:c1", "Bash", "execute", &arguments) {
                    emit(stdout, &frame);
                }
                emit(
                    stdout,
                    &permission_request("perm-shell", "1:c1", "Bash", "Running: rm -rf build"),
                );
            }
            "classify write" => {
                // Kimi's Write display is `file_io` without before/after, so the
                // request carries no diff card: the path exists only in the
                // arguments streamed into the call's card.
                let arguments = json!({"path":"/home/dev/.zshrc","content":"export EDITOR=vim\n"});
                for frame in streamed_call("1:c2", "Write", "edit", &arguments) {
                    emit(stdout, &frame);
                }
                emit(
                    stdout,
                    &permission_request("perm-write", "1:c2", "Write", "Writing /home/dev/.zshrc"),
                );
            }
            "surrogate" => {
                // Kimi's Bash summary is `Running: ` + the command's first 50
                // UTF-16 units. Here that cut splits the emoji, and Node writes
                // its first half as a lone `\ud83d` escape.
                let command = "git commit -m \"修复登录页面在移动端的布局错位问题并优化了加载速度与交互体验 🎉\"";
                for frame in streamed_call("1:s1", "Bash", "execute", &json!({"command":command})) {
                    emit(stdout, &frame);
                }
                let cut = "Running: git commit -m \"修复登录页面在移动端的布局错位问题并优化了加载速度与交互体 LONE…";
                let request = permission_request("perm-surrogate", "1:s1", "Bash", cut).to_string();
                assert!(request.contains("LONE"));
                writeln!(stdout, "{}", request.replace("LONE", "\\ud83d")).unwrap();
                stdout.flush().unwrap();
            }
            "unparseable" => {
                // Intact JSON-RPC framing around a value no JSON number can hold.
                let request = permission_request("perm-unparseable", "1:u1", "Bash", "Running: ls")
                    .to_string()
                    .replacen("\"params\":{", "\"params\":{\"n\":1e999999,", 1);
                assert!(request.contains("1e999999"));
                writeln!(stdout, "{request}").unwrap();
                stdout.flush().unwrap();
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

async fn start_scripted_kimi(workspace: &Path) -> AcpSession {
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
        BasePermissionProfile::Guarded,
        None,
    )
    .await
    .unwrap()
}

/// The next event, or a panic when none arrives in time.
async fn next_event_within(
    session: &mut AcpSession,
    deadline: tokio::time::Instant,
) -> SessionEvent {
    tokio::time::timeout_at(deadline, session.next_event())
        .await
        .expect("the Kimi fixture did not answer in time")
        .expect("the Kimi fixture session closed")
}

/// The next approval the session surfaces, as `(req_id, action, target)`.
async fn next_approval(session: &mut AcpSession) -> (String, String, String) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let SessionEvent::HostRequest {
            req_id,
            request: HostRequest::Approval { action, target, .. },
        } = next_event_within(session, deadline).await
        {
            return (req_id, action, target);
        }
    }
}

async fn next_turn_status(session: &mut AcpSession) -> TurnStatus {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let SessionEvent::TurnDone { status, .. } = next_event_within(session, deadline).await {
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

/// What UmaDev answered to the fixture's request `id`.
fn reply(workspace: &Path, id: &str) -> Value {
    let reply = std::fs::read_to_string(workspace.join(format!("reply-{id}.json"))).unwrap();
    serde_json::from_str(&reply).unwrap()
}

#[tokio::test]
async fn kimi_permission_request_is_classified_by_the_correlated_tool_call() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = start_scripted_kimi(workspace.path()).await;
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

/// A Kimi permission request for `call_id`, as `buildPermissionToolCallUpdate`
/// shapes it.
fn summary_request(call_id: &str, tool: &str, summary: &str) -> Value {
    json!({
        "toolCallId":call_id, "title":tool,
        "content":text_content(&format!("Requesting approval to {summary}"))
    })
}

fn replay(tools: &mut ToolState, frames: impl IntoIterator<Item = Value>) {
    for frame in frames {
        parse_session_update(&frame["params"], tools);
    }
}

#[test]
fn approval_subject_prefers_the_call_arguments_over_the_summary() {
    let mut tools = ToolState::default();

    // Kimi: the arguments streamed into a lazily created card.
    let write = json!({"path":"src/lib.rs","content":"x"});
    replay(&mut tools, streamed_call("2:w1", "Write", "edit", &write));
    assert_eq!(
        approval_subject(
            &summary_request("2:w1", "Write", "Writing src/lib.rs"),
            &tools
        ),
        ("Edit".to_string(), "src/lib.rs".to_string())
    );

    // An agent that announces a call with its parsed arguments before asking.
    replay(
        &mut tools,
        [session_update(&json!({
            "sessionUpdate":"tool_call", "toolCallId":"2:b1", "title":"Run tests",
            "kind":"execute", "status":"in_progress", "rawInput":{"command":"cargo test"}
        }))],
    );
    assert_eq!(
        approval_subject(&json!({"toolCallId":"2:b1","title":"Run tests"}), &tools),
        ("Bash".to_string(), "cargo test".to_string())
    );

    // A request that carries its own kind and arguments is judged by them.
    let own = json!({
        "toolCallId":"2:b1", "title":"Run", "kind":"execute", "rawInput":{"command":"make"}
    });
    assert_eq!(
        approval_subject(&own, &tools),
        ("Bash".to_string(), "make".to_string())
    );

    // A settled call is forgotten, leaving only the request's own summary.
    replay(
        &mut tools,
        [session_update(&json!({
            "sessionUpdate":"tool_call_update", "toolCallId":"2:w1",
            "status":"completed", "rawOutput":"ok"
        }))],
    );
    assert_eq!(
        approval_subject(
            &summary_request("2:w1", "Write", "Writing src/lib.rs"),
            &tools
        ),
        (
            "Write".to_string(),
            "Requesting approval to Writing src/lib.rs".to_string()
        )
    );
}

#[test]
fn approval_subject_never_judges_a_command_by_its_summary() {
    let mut tools = ToolState::default();

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

    // An unannounced command is never judged by its summary, which Kimi cuts
    // after 50 UTF-16 units: with nothing to inspect, the trust floor asks.
    let hidden = "Running: echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx…";
    assert_eq!(
        approval_subject(&summary_request("2:x1", "Bash", hidden), &tools),
        ("Bash".to_string(), String::new())
    );

    // Arguments still streaming are no input yet.
    replay(
        &mut tools,
        [session_update(&json!({
            "sessionUpdate":"tool_call", "toolCallId":"2:p1", "title":"Bash",
            "kind":"execute", "status":"pending", "content":text_content("{\"command\":\"rm")
        }))],
    );
    assert_eq!(
        approval_subject(&summary_request("2:p1", "Bash", "Running: rm"), &tools),
        ("Bash".to_string(), String::new())
    );

    // Calls that never settle age out, oldest first.
    for index in 0..MAX_TOOL_CALL_SUBJECTS {
        let arguments = json!({"command":format!("step {index}")});
        replay(
            &mut tools,
            streamed_call(&format!("3:c{index}"), "Bash", "execute", &arguments),
        );
    }
    assert_eq!(tools.subjects.len(), MAX_TOOL_CALL_SUBJECTS);
    assert!(!tools.subjects.contains_key("2:p1"));
    assert_eq!(
        approval_subject(
            &summary_request("3:c63", "Bash", "Running: step 63"),
            &tools
        )
        .1,
        "step 63"
    );
}

#[tokio::test]
async fn unpaired_surrogate_permission_request_is_still_answered() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = start_scripted_kimi(workspace.path()).await;
    session.send_turn("surrogate".to_string()).await.unwrap();
    // The request carries a lone `\ud83d`. Dropping it would leave Kimi's
    // permission RPC, which has no timeout, waiting for good.
    let (req_id, action, target) = next_approval(&mut session).await;
    assert_eq!(action, "Bash");
    assert_eq!(
        target,
        "git commit -m \"修复登录页面在移动端的布局错位问题并优化了加载速度与交互体验 🎉\""
    );
    deny(&mut session, &req_id).await;
    assert_eq!(next_turn_status(&mut session).await, TurnStatus::Completed);
    assert_eq!(
        reply(workspace.path(), "perm-surrogate"),
        json!({"outcome":{"outcome":"selected","optionId":"reject"}})
    );
    session.end().await.unwrap();
}

#[test]
fn only_unpaired_surrogate_escapes_are_repaired() {
    // A lone half of either kind becomes U+FFFD; the rest is kept exactly.
    assert_eq!(
        parse_peer_frame(r#"{"a":"x\ud83d…","b":"\ude00y"}"#).unwrap(),
        json!({"a":"x\u{fffd}…","b":"\u{fffd}y"})
    );
    // A pair, an escaped backslash before `u` and other escapes are untouched.
    for intact in [
        r#"{"a":"\ud83d\ude00"}"#,
        r#"{"a":"\\ud83d"}"#,
        r#"{"a":"\"\n\u0041"}"#,
    ] {
        assert_eq!(replace_unpaired_surrogate_escapes(intact), None, "{intact}");
    }
    assert_eq!(
        parse_peer_frame(r#"{"a":"\\ud83d"}"#).unwrap(),
        json!({"a":"\\ud83d"})
    );
    // A high half followed by another escape is still unpaired.
    assert_eq!(
        replace_unpaired_surrogate_escapes(r#""\uD83D\u0041""#).as_deref(),
        Some(r#""\ufffd\u0041""#)
    );
    // A line broken in any other way still fails.
    assert!(parse_peer_frame(r#"{"a":"\ud83d"#).is_err());
}

#[tokio::test]
async fn unparseable_permission_request_is_answered_not_dropped() {
    let workspace = tempfile::tempdir().unwrap();
    let mut session = start_scripted_kimi(workspace.path()).await;
    session.send_turn("unparseable".to_string()).await.unwrap();
    // The request cannot be shown faithfully, so it is declined as cancelled
    // instead of left waiting; the turn then ends normally.
    assert_eq!(next_turn_status(&mut session).await, TurnStatus::Completed);
    assert_eq!(
        reply(workspace.path(), "perm-unparseable"),
        json!({"outcome":{"outcome":"cancelled"}})
    );
    session.end().await.unwrap();
}
