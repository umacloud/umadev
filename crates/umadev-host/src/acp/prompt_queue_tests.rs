use super::*;

use std::io::{BufRead as _, Write as _};

fn emit(stdout: &mut std::io::Stdout, value: impl Into<Value>) {
    let value = value.into();
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

fn changed(entries: impl Into<Value>, running_prompt_id: Option<&str>) -> Value {
    let entries = entries.into();
    let mut params = json!({
        "sessionId":"queue-session",
        "entries":entries,
    });
    if let Some(prompt_id) = running_prompt_id {
        params["runningPromptId"] = Value::String(prompt_id.to_string());
    }
    json!({
        "jsonrpc":"2.0",
        "method":"_x.ai/queue/changed",
        "params":params,
    })
}

#[derive(Default)]
struct QueueFixtureState {
    first_rpc: Option<Value>,
    first_prompt: Option<String>,
    second_rpc: Option<Value>,
    second_prompt: Option<String>,
    discard_rpc: Option<Value>,
    discard_prompt: Option<String>,
    urgent_rpc: Option<Value>,
}

impl QueueFixtureState {
    fn handle_prompt(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
        let text = frame["params"]["prompt"][0]["text"].as_str().unwrap();
        let prompt_id = frame["params"]["_meta"]["promptId"]
            .as_str()
            .unwrap()
            .to_string();
        match text {
            "first" => {
                assert!(frame["params"]["_meta"].get("sendNow").is_none());
                self.first_rpc = frame.get("id").cloned();
                self.first_prompt = Some(prompt_id.clone());
                emit(stdout, changed(json!([]), Some(&prompt_id)));
            }
            "second" => {
                assert!(frame["params"]["_meta"].get("sendNow").is_none());
                self.second_rpc = frame.get("id").cloned();
                self.second_prompt = Some(prompt_id.clone());
                emit(
                    stdout,
                    changed(
                        json!([{
                            "id":prompt_id,"version":0,"owner":"umadev",
                            "kind":"prompt","text":"second","position":0
                        }]),
                        self.first_prompt.as_deref(),
                    ),
                );
            }
            "discard" => self.handle_discard_prompt(frame, &prompt_id, stdout),
            "urgent" => self.handle_urgent_prompt(frame, &prompt_id, stdout),
            other => panic!("unexpected queued prompt {other}"),
        }
    }

    fn handle_discard_prompt(
        &mut self,
        frame: &Value,
        prompt_id: &str,
        stdout: &mut std::io::Stdout,
    ) {
        assert!(frame["params"]["_meta"].get("sendNow").is_none());
        self.discard_rpc = frame.get("id").cloned();
        self.discard_prompt = Some(prompt_id.to_string());
        emit(
            stdout,
            changed(
                json!([
                    {"id":self.second_prompt.as_deref().unwrap(),"version":0,"owner":"umadev","kind":"prompt","text":"second","position":0},
                    {"id":prompt_id,"version":0,"owner":"umadev","kind":"prompt","text":"discard","position":1}
                ]),
                self.first_prompt.as_deref(),
            ),
        );
    }

    fn handle_urgent_prompt(
        &mut self,
        frame: &Value,
        prompt_id: &str,
        stdout: &mut std::io::Stdout,
    ) {
        assert_eq!(frame["params"]["_meta"]["sendNow"], true);
        self.urgent_rpc = frame.get("id").cloned();
        emit(
            stdout,
            changed(
                json!([{
                    "id":self.second_prompt.as_deref().unwrap(),"version":1,
                    "owner":"umadev","lastEditor":"umadev","kind":"prompt",
                    "text":"second edited","position":0
                }]),
                Some(prompt_id),
            ),
        );
        emit(
            stdout,
            json!({
                "jsonrpc":"2.0","id":self.first_rpc.take().unwrap(),
                "result":{"stopReason":"cancelled"}
            }),
        );
    }

    fn handle_mutation(&mut self, method: &str, frame: &Value, stdout: &mut std::io::Stdout) {
        match method {
            "_x.ai/queue/remove" => self.handle_remove(frame, stdout),
            "_x.ai/queue/edit" => self.handle_edit(frame, stdout),
            "_x.ai/queue/reorder" => self.handle_reorder(frame, stdout),
            "_x.ai/queue/clear" => self.handle_clear(frame, stdout),
            _ => {}
        }
    }

    fn handle_remove(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
        assert_eq!(
            frame["params"]["id"].as_str(),
            self.discard_prompt.as_deref()
        );
        assert_eq!(frame["params"]["expectedVersion"], 0);
        emit(
            stdout,
            json!({
                "jsonrpc":"2.0","id":self.discard_rpc.take().unwrap(),
                "result":{"stopReason":"cancelled"}
            }),
        );
        emit(
            stdout,
            changed(
                json!([{
                    "id":self.second_prompt.as_deref().unwrap(),"version":0,
                    "owner":"umadev","kind":"prompt","text":"second","position":0
                }]),
                self.first_prompt.as_deref(),
            ),
        );
    }

    fn handle_edit(&self, frame: &Value, stdout: &mut std::io::Stdout) {
        assert_eq!(
            frame["params"]["id"].as_str(),
            self.second_prompt.as_deref()
        );
        assert_eq!(frame["params"]["newText"], "second edited");
        emit(
            stdout,
            changed(
                json!([{
                    "id":self.second_prompt.as_deref().unwrap(),"version":1,
                    "owner":"umadev","lastEditor":"umadev","kind":"prompt",
                    "text":"second edited","position":0
                }]),
                self.first_prompt.as_deref(),
            ),
        );
    }

    fn handle_reorder(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
        assert_eq!(
            frame["params"]["orderedIds"],
            json!([self.second_prompt.as_deref().unwrap()])
        );
        emit(
            stdout,
            json!({
                "jsonrpc":"2.0","id":self.urgent_rpc.take().unwrap(),
                "result":{"stopReason":"end_turn"}
            }),
        );
        emit(stdout, changed(json!([]), self.second_prompt.as_deref()));
    }

    fn handle_clear(&mut self, frame: &Value, stdout: &mut std::io::Stdout) {
        assert_eq!(frame["params"]["sessionId"], "queue-session");
        emit(
            stdout,
            json!({
                "jsonrpc":"2.0","id":self.second_rpc.take().unwrap(),
                "result":{"stopReason":"end_turn"}
            }),
        );
        emit(stdout, changed(json!([]), None));
    }
}

#[test]
fn fake_grok_prompt_queue_child() {
    let invoked = std::env::args().any(|arg| arg == "--exact")
        && std::env::args().any(|arg| arg.ends_with("fake_grok_prompt_queue_child"));
    if !invoked {
        return;
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    writeln!(stdout).unwrap();
    stdout.flush().unwrap();

    let mut state = QueueFixtureState::default();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let frame: Value = serde_json::from_str(&line).unwrap();
        match frame.get("method").and_then(Value::as_str) {
            Some("initialize") => emit(
                &mut stdout,
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
                }),
            ),
            Some("session/new") => emit(
                &mut stdout,
                json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"sessionId":"queue-session"}
                }),
            ),
            Some("session/set_mode") => emit(
                &mut stdout,
                json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}),
            ),
            Some("session/prompt") => state.handle_prompt(&frame, &mut stdout),
            Some(
                method @ ("_x.ai/queue/remove"
                | "_x.ai/queue/edit"
                | "_x.ai/queue/reorder"
                | "_x.ai/queue/clear"),
            ) => {
                state.handle_mutation(method, &frame, &mut stdout);
            }
            Some("_x.ai/session/close") => emit(
                &mut stdout,
                json!({
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
        .expect("queue event timed out")
        .expect("queue session closed")
}

async fn start_prompt_queue_session() -> AcpSession {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let args = vec![
        "--exact".to_string(),
        "acp::prompt_queue_tests::fake_grok_prompt_queue_child".to_string(),
        "--nocapture".to_string(),
        "--test-threads=1".to_string(),
    ];
    let session = AcpSession::start_with_program_args(
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
        .supports(SessionCapability::PromptQueue));
    session
}

async fn exercise_queue_edits(session: &mut AcpSession) -> String {
    session.send_turn("first".to_string()).await.unwrap();
    assert!(matches!(
        next_with_timeout(&mut *session).await,
        SessionEvent::PromptQueueChanged(PromptQueueSnapshot {
            ref entries,
            running_prompt_id: Some(_),
            ..
        }) if entries.is_empty()
    ));

    let queued = session
        .enqueue_input(TurnInput::text("second"), PromptQueuePlacement::Tail)
        .await
        .unwrap();
    assert_eq!(queued.receipt, DeliveryReceiptStage::TransportWritten);
    let second_id = match next_with_timeout(&mut *session).await {
        SessionEvent::PromptQueueChanged(snapshot) => {
            assert_eq!(snapshot.entries.len(), 1);
            assert_eq!(snapshot.entries[0].text, "second");
            snapshot.entries[0].id.clone()
        }
        other => panic!("expected queue snapshot, got {other:?}"),
    };

    session
        .enqueue_input(TurnInput::text("discard"), PromptQueuePlacement::Tail)
        .await
        .unwrap();
    let discard_id = match next_with_timeout(&mut *session).await {
        SessionEvent::PromptQueueChanged(snapshot) => {
            assert_eq!(snapshot.entries.len(), 2);
            snapshot.entries[1].id.clone()
        }
        other => panic!("expected queue snapshot, got {other:?}"),
    };
    session
        .mutate_prompt_queue(PromptQueueMutation::Remove {
            id: discard_id,
            expected_version: 0,
        })
        .await
        .unwrap();
    assert!(matches!(
        next_with_timeout(&mut *session).await,
        SessionEvent::PromptQueueChanged(PromptQueueSnapshot { ref entries, .. })
            if entries.len() == 1 && entries[0].id == second_id
    ));

    session
        .mutate_prompt_queue(PromptQueueMutation::Edit {
            id: second_id.clone(),
            new_text: "second edited".to_string(),
        })
        .await
        .unwrap();
    assert!(matches!(
        next_with_timeout(&mut *session).await,
        SessionEvent::PromptQueueChanged(PromptQueueSnapshot { ref entries, .. })
            if entries.len() == 1
                && entries[0].id == second_id
                && entries[0].version == 1
                && entries[0].text == "second edited"
    ));
    second_id
}

async fn exercise_send_now_and_reorder(session: &mut AcpSession, second_id: &str) {
    session
        .enqueue_input(TurnInput::text("urgent"), PromptQueuePlacement::SendNow)
        .await
        .unwrap();
    assert!(matches!(
        next_with_timeout(&mut *session).await,
        SessionEvent::PromptQueueChanged(PromptQueueSnapshot {
            running_prompt_id: Some(_),
            ref entries,
            ..
        }) if entries.len() == 1 && entries[0].id == second_id
    ));
    assert_eq!(
        next_with_timeout(&mut *session).await,
        SessionEvent::TurnDone {
            status: TurnStatus::Interrupted,
            usage: None,
        }
    );

    session
        .mutate_prompt_queue(PromptQueueMutation::Reorder {
            ordered_ids: vec![second_id.to_string()],
        })
        .await
        .unwrap();
    let first = next_with_timeout(&mut *session).await;
    let second = next_with_timeout(&mut *session).await;
    let pair = [&first, &second];
    assert!(pair.iter().any(|event| {
        matches!(
            event,
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }
        )
    }));
    assert!(pair.iter().any(|event| {
        matches!(
            event,
            SessionEvent::PromptQueueChanged(PromptQueueSnapshot {
                running_prompt_id: Some(id),
                entries,
                ..
            }) if id == second_id && entries.is_empty()
        )
    }));
}

async fn exercise_queue_clear(session: &mut AcpSession) {
    session
        .mutate_prompt_queue(PromptQueueMutation::Clear)
        .await
        .unwrap();
    let first = next_with_timeout(&mut *session).await;
    let second = next_with_timeout(&mut *session).await;
    let pair = [&first, &second];
    assert!(pair.iter().any(|event| {
        matches!(
            event,
            SessionEvent::TurnDone {
                status: TurnStatus::Completed,
                usage: None,
            }
        )
    }));
    assert!(pair.iter().any(|event| {
        matches!(
            event,
            SessionEvent::PromptQueueChanged(PromptQueueSnapshot {
                running_prompt_id: None,
                entries,
                ..
            }) if entries.is_empty()
        )
    }));
}

#[tokio::test]
async fn grok_prompt_queue_is_server_authoritative_and_drains_each_prompt_once() {
    let mut session = start_prompt_queue_session().await;
    let second_id = exercise_queue_edits(&mut session).await;
    exercise_send_now_and_reorder(&mut session, &second_id).await;
    exercise_queue_clear(&mut session).await;
    session.end().await.unwrap();
}

#[test]
fn fake_unverified_future_grok_child() {
    let invoked = std::env::args().any(|arg| arg == "--exact")
        && std::env::args().any(|arg| arg.ends_with("fake_unverified_future_grok_child"));
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
                assert!(
                    frame["params"]["clientCapabilities"].get("_meta").is_none(),
                    "future Grok received an unverified private client claim"
                );
                emit(
                    &mut stdout,
                    json!({
                        "jsonrpc":"2.0", "id":frame["id"],
                        "result":{
                            "protocolVersion":1,
                            "agentCapabilities":{"loadSession":true},
                            "_meta":{
                                "grokShell":true,
                                "agentVersion":"99.7.3+future.adapter"
                            }
                        }
                    }),
                );
            }
            Some("session/new") => emit(
                &mut stdout,
                json!({
                    "jsonrpc":"2.0", "id":frame["id"],
                    "result":{"sessionId":"future-standard-session"}
                }),
            ),
            Some(method) => {
                std::fs::write("unexpected-future-private-method", method).unwrap();
            }
            None => {}
        }
    }
}

#[tokio::test]
async fn unverified_future_grok_never_writes_queue_or_other_private_controls() {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let args = vec![
        "--exact".to_string(),
        "acp::prompt_queue_tests::fake_unverified_future_grok_child".to_string(),
        "--nocapture".to_string(),
        "--test-threads=1".to_string(),
    ];
    let mut session = AcpSession::start_with_program_args_and_firmware_and_policy(
        AcpVendor::Grok,
        executable.to_str().unwrap(),
        args,
        workspace.path(),
        "",
        BasePermissionProfile::Guarded,
        None,
        None,
        SessionOpenPolicy::NonInteractive,
        FolderTrustClientSurface::Headless,
        Some(GrokSourceCapabilities::NONE),
    )
    .await
    .map_err(legacy_session_open_error)
    .unwrap();

    let capabilities = session.capabilities();
    assert_eq!(capabilities.image_input, InputDelivery::Unsupported);
    assert_eq!(capabilities.steer, SteerSemantics::Unsupported);
    assert_eq!(
        capabilities.prompt_queue,
        PromptQueueCapability::Unsupported
    );
    assert_eq!(
        capabilities.background_process_control,
        BackgroundProcessControlCapability::Unsupported
    );
    assert!(!capabilities.set_mode);

    assert!(matches!(
        session
            .enqueue_input(
                TurnInput::text("must stay local"),
                PromptQueuePlacement::Tail
            )
            .await,
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::PromptQueue
        ))
    ));
    assert!(matches!(
        session
            .mutate_prompt_queue(PromptQueueMutation::Clear)
            .await,
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::PromptQueue
        ))
    ));
    assert!(matches!(
        session.steer("must not interject".to_string()).await,
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::MidTurnSteer
        ))
    ));
    assert!(matches!(
        session.list_background_processes().await,
        Err(SessionError::CapabilityUnsupported(
            SessionCapability::BackgroundProcessControl
        ))
    ));
    session.end().await.unwrap();

    assert!(
        !workspace
            .path()
            .join("unexpected-future-private-method")
            .exists(),
        "a capability guard leaked an unverified private method to Grok"
    );
}
