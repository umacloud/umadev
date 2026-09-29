//! Grok `ask_user_question` answers, replayed by a scripted child process.

use super::*;

use std::io::{BufRead as _, Write as _};

const SESSION: &str = "grok-question-session";
const DATABASE_QUESTION: &str = "Use postgres://postgres@localhost/app?";
const REMOTE_QUESTION: &str = "Which remote should I push to?";
const SSH_REMOTE: &str = "ssh://git@github.com/acme/app.git";
const DATABASE_PREVIEW: &str = "DATABASE_URL=postgres://postgres@localhost/app";

fn emit(stdout: &mut std::io::Stdout, value: &Value) {
    writeln!(stdout, "{value}").unwrap();
    stdout.flush().unwrap();
}

/// Grok's direct-stdio `ask_user_question` with text the host redacts for
/// display: URI userinfo in a question, in an option without an id (whose
/// label is its value), and in a preview.
fn ask_user_question() -> Value {
    json!({
        "jsonrpc":"2.0", "id":"ask-1", "method":"_x.ai/ask_user_question",
        "params":{
            "sessionId":SESSION, "toolCallId":"tool-ask", "mode":"default",
            "questions":[
                {
                    "id":"q1", "question":DATABASE_QUESTION, "multiSelect":false,
                    "options":[
                        {"id":"y","label":"Yes","description":"Connect","preview":DATABASE_PREVIEW},
                        {"id":"n","label":"No","description":"Ask again"}
                    ]
                },
                {
                    "id":"q2", "question":REMOTE_QUESTION, "multiSelect":false,
                    "options":[
                        {"label":SSH_REMOTE,"description":"SSH"},
                        {"label":"https://github.com/acme/app.git","description":"HTTPS"}
                    ]
                }
            ]
        }
    })
}

#[test]
fn fake_grok_question_child() {
    let invoked = std::env::args().any(|arg| arg == "--exact")
        && std::env::args().any(|arg| arg.ends_with("fake_grok_question_child"));
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
            Some("initialize") => emit(
                &mut stdout,
                &json!({
                    "jsonrpc":"2.0", "id":frame["id"], "result":{
                        "protocolVersion":1, "agentCapabilities":{},
                        "_meta":{
                            "grokShell":true,
                            "agentVersion":crate::grok_contract::GROK_BUILD_SOURCE_VERSION
                        }
                    }
                }),
            ),
            Some("session/new") => emit(
                &mut stdout,
                &json!({"jsonrpc":"2.0", "id":frame["id"], "result":{"sessionId":SESSION}}),
            ),
            Some("session/set_mode") => emit(
                &mut stdout,
                &json!({"jsonrpc":"2.0", "id":frame["id"], "result":{}}),
            ),
            Some("session/prompt") => {
                prompt_id = frame.get("id").cloned();
                emit(&mut stdout, &ask_user_question());
            }
            None if frame.get("id") == Some(&json!("ask-1")) => {
                std::fs::write("ask-1-reply.json", frame["result"].to_string()).unwrap();
                if let Some(id) = prompt_id.take() {
                    emit(
                        &mut stdout,
                        &json!({"jsonrpc":"2.0", "id":id, "result":{"stopReason":"end_turn"}}),
                    );
                }
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn grok_answer_keys_are_the_original_question_text() {
    let executable = std::env::current_exe().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let mut session = AcpSession::start_with_program_args(
        AcpVendor::Grok,
        executable.to_str().unwrap(),
        vec![
            "--exact".to_string(),
            "acp::grok_question_tests::fake_grok_question_child".to_string(),
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ],
        workspace.path(),
        "",
        BasePermissionProfile::Guarded,
        None,
    )
    .await
    .unwrap();
    session.send_turn("ask".to_string()).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let (req_id, questions) = loop {
        let event = tokio::time::timeout_at(deadline, session.next_event())
            .await
            .expect("the question did not surface in time")
            .expect("the Grok fixture session closed");
        if let SessionEvent::HostRequest {
            req_id,
            request: HostRequest::UserInput { questions, .. },
        } = event
        {
            break (req_id, questions);
        }
    };

    // The user is shown the redacted text and answers with what was shown.
    let [database, remote] = questions.as_slice() else {
        panic!("expected both questions: {questions:?}");
    };
    assert!(
        !database.prompt.contains("postgres@"),
        "{}",
        database.prompt
    );
    let yes = &database.options[0];
    assert!(!yes.preview.as_deref().unwrap().contains("postgres@"));
    let ssh = &remote.options[0];
    assert!(!ssh.value.contains("git@"), "{}", ssh.value);
    session
        .respond_host(
            &req_id,
            HostResponse::UserInputOutcome {
                outcome: HostUserInputOutcome::Accepted {
                    answers: vec![
                        HostAnswer {
                            question_id: database.id.clone(),
                            values: vec![yes.value.clone()],
                        },
                        HostAnswer {
                            question_id: remote.id.clone(),
                            values: vec![ssh.value.clone()],
                        },
                    ],
                    annotations: vec![HostQuestionAnnotation {
                        question_id: database.id.clone(),
                        preview: yes.preview.clone(),
                        notes: None,
                    }],
                },
            },
        )
        .await
        .unwrap();
    let status = loop {
        let event = tokio::time::timeout_at(deadline, session.next_event())
            .await
            .expect("the turn did not finish in time")
            .expect("the Grok fixture session closed");
        if let SessionEvent::TurnDone { status, .. } = event {
            break status;
        }
    };
    assert_eq!(status, TurnStatus::Completed);

    // Grok matches answers by its own question text and option labels, so
    // they go back exactly as it sent them.
    let reply = std::fs::read_to_string(workspace.path().join("ask-1-reply.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&reply).unwrap(),
        json!({
            "outcome":"accepted",
            "answers":{
                DATABASE_QUESTION:["Yes"],
                REMOTE_QUESTION:[SSH_REMOTE]
            },
            "annotations":{DATABASE_QUESTION:{"preview":DATABASE_PREVIEW}}
        })
    );
    session.end().await.unwrap();
}
