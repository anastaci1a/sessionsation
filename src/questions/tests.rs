use super::*;
use futures_util::{SinkExt, StreamExt};
use std::io::Write;
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::Message;

fn fixture() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    fs::create_dir_all(&paths.root).unwrap();
    (dir, paths)
}

fn append(path: &Path, lines: &[Value]) {
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn call(name: &str, id: &str, arguments: Value) -> Value {
    json!({"timestamp":"2026-10-29T14:53:59.647Z","type":"response_item","payload":{"type":"function_call",
        "name":name,"arguments":arguments.to_string(),"call_id":id,
        "internal_chat_message_metadata_passthrough":{"turn_id":"turn-1"}}})
}
fn output(id: &str) -> Value {
    json!({"timestamp":"2026-10-29T14:54:00.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":id,"output":"{}"}})
}
fn event(kind: &str) -> Value {
    json!({"timestamp":"2026-10-29T14:55:00.000Z","type":"event_msg","payload":{"type":kind}})
}
fn blocking_args() -> Value {
    json!({"questions":[
        {"id":"color","header":"Color","question":"Pick a color","options":[{"label":"Red","description":"r"},{"label":"Blue","description":"b"}]},
        {"id":"name","header":"Name","question":"Type any word","options":[{"label":"Alpha","description":"a"},{"label":"Beta","description":"b"}]}]})
}

#[test]
fn codex_blocking_question_is_pending_until_its_output_and_scans_incrementally() {
    let (dir, paths) = fixture();
    let rollout = dir.path().join("rollout.jsonl");
    append(
        &rollout,
        &[
            event("task_started"),
            call("request_user_input", "call_1", blocking_args()),
        ],
    );
    let open = scan(&paths, Format::Codex, &rollout).unwrap();
    assert_eq!(open.len(), 1);
    let p = &open[0];
    assert_eq!(
        (p.request_id.as_str(), p.kind.as_str(), p.blocking),
        ("call_1", KIND_CODEX_BLOCKING, true)
    );
    assert_eq!(p.asked_at, 1_793_285_639);
    assert_eq!(p.turn_id.as_deref(), Some("turn-1"));
    assert_eq!(p.questions[0].id, "color");
    assert_eq!(
        p.questions[0].options[1],
        QuestionOption {
            id: "o2".into(),
            label: "Blue".into(),
            description: Some("b".into())
        }
    );
    assert!(p.questions[0].allow_free_text && !p.questions[0].multi);
    // A partial record is not consumed; completing it later is.
    let mut f = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    let line = output("call_1").to_string();
    write!(f, "{}", &line[..10]).unwrap();
    assert_eq!(scan(&paths, Format::Codex, &rollout).unwrap().len(), 1);
    writeln!(f, "{}", &line[10..]).unwrap();
    assert!(scan(&paths, Format::Codex, &rollout).unwrap().is_empty());
    // An aborted turn also ends a blocking request.
    append(
        &rollout,
        &[
            call("request_user_input", "call_2", blocking_args()),
            event("turn_aborted"),
        ],
    );
    assert!(scan(&paths, Format::Codex, &rollout).unwrap().is_empty());
}

#[test]
fn codex_async_question_survives_its_turn_and_is_superseded_by_a_new_turn() {
    let (dir, paths) = fixture();
    let rollout = dir.path().join("rollout.jsonl");
    let args = json!({"questions":[{"title":"Which inputs?","options":["Synthetic (recommended)","Representative"]},{"title":"Anything else?"}]});
    append(
        &rollout,
        &[
            call("request_user_input_async", "call_a", args),
            output("call_a"),
            event("task_complete"),
        ],
    );
    let open = scan(&paths, Format::Codex, &rollout).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].kind, KIND_CODEX_ASYNC);
    assert!(!open[0].blocking);
    assert_eq!(open[0].questions[1].id, "q2");
    assert!(open[0].questions[1].options.is_empty() && open[0].questions[1].allow_free_text);
    // A replaced file is rescanned from the start.
    fs::remove_file(&rollout).unwrap();
    append(&rollout, &[event("task_started")]);
    assert!(scan(&paths, Format::Codex, &rollout).unwrap().is_empty());
}

#[test]
fn claude_ask_user_question_pending_until_tool_result_and_multi_select() {
    let (dir, paths) = fixture();
    let t = dir.path().join("t.jsonl");
    append(
        &t,
        &[
            json!({"type":"assistant","timestamp":"2026-10-29T14:53:59Z","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"AskUserQuestion",
        "input":{"questions":[{"question":"Which features?","header":"Features","multiSelect":true,"options":[{"label":"A","description":"a"},{"label":"B","description":"b"}]}]}}]}}),
        ],
    );
    let open = scan(&paths, Format::Claude, &t).unwrap();
    assert_eq!(open.len(), 1);
    assert!(open[0].questions[0].multi);
    let answers = parse_answers(r#"{"q1":{"option_ids":["o1","o2"],"text":"also C"}}"#).unwrap();
    validate(&open[0].questions, &answers).unwrap();
    let input = json!({"questions":[{"question":"Which features?"}]});
    let updated = claude_updated_input(&input, &open[0].questions, &answers);
    assert_eq!(updated["answers"]["Which features?"], "A, B, also C");
    assert_eq!(updated["questions"], input["questions"]);
    append(
        &t,
        &[
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"x"}]}}),
        ],
    );
    assert!(scan(&paths, Format::Claude, &t).unwrap().is_empty());
    // Terminal Claude sessions are read-only with an honest reason.
    append(
        &t,
        &[
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"AskUserQuestion","input":{"questions":[{"question":"Q?","options":[]}]}}]}}),
        ],
    );
    let rows = claude_pending(
        &paths,
        "s",
        "11111111-1111-4111-8111-111111111111",
        Some(&t),
    );
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].answerable);
    assert!(
        rows[0]
            .unanswerable_reason
            .as_deref()
            .unwrap()
            .starts_with("PROVIDER_TERMINAL_ONLY")
    );
    let a = parse_answers(r#"{"q1":{"text":"x"}}"#).unwrap();
    let err = admit(&paths, "s", rows.first(), "toolu_2", &a)
        .err()
        .unwrap()
        .to_string();
    assert!(err.starts_with("PROVIDER_TERMINAL_ONLY"), "{err}");
}

#[test]
fn answers_are_validated_and_encoded_per_provider() {
    let q = codex_blocking_questions(&blocking_args());
    let ok = parse_answers(r#"{"color":{"option_ids":["o2"]},"name":{"text":"zebra"}}"#).unwrap();
    validate(&q, &ok).unwrap();
    assert_eq!(
        codex_response(&q, &ok),
        json!({"answers":{"color":{"answers":["Blue"]},"name":{"answers":["user_note: zebra"]}}})
    );
    for bad in [
        r#"{"color":{"option_ids":["o2"]}}"#,
        r#"{"color":{"option_ids":["o1","o2"]},"name":{"text":"x"}}"#,
        r#"{"color":{"option_ids":["o9"]},"name":{"text":"x"}}"#,
        r#"{"color":{"option_ids":[]},"name":{"text":"x"}}"#,
        r#"{"color":{"option_ids":["o1"]},"name":{"text":"x"},"other":{"text":"x"}}"#,
    ] {
        let a = parse_answers(bad).unwrap();
        assert!(
            validate(&q, &a)
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_ANSWERS"),
            "{bad}"
        );
    }
    assert!(parse_answers(r#"{"color":{"option":"o1"}}"#).is_err());
    let async_q = codex_async_questions(
        &json!({"questions":[{"title":"Pick a color","options":["Red","Blue"]}]}),
    );
    let a = parse_answers(r#"{"q1":{"option_ids":["o1"]}}"#).unwrap();
    assert_eq!(
        async_message(&async_q, &a),
        "Answer to your request_user_input_async question:\n1. Q: Pick a color\n   A: Red\n"
    );
}

fn pending_blocking() -> Pending {
    Pending {
        request_id: "call_x".into(),
        asked_at: 1,
        kind: KIND_CODEX_BLOCKING.into(),
        blocking: true,
        turn_id: None,
        questions: codex_blocking_questions(&blocking_args()),
        answerable: true,
        unanswerable_reason: None,
    }
}

#[test]
fn a_stale_question_and_a_double_answer_are_definite_rejections() {
    let (_dir, paths) = fixture();
    let answers = parse_answers(r#"{"color":{"option_ids":["o1"]},"name":{"text":"a"}}"#).unwrap();
    let err = admit(&paths, "s", None, "call_x", &answers)
        .err()
        .unwrap()
        .to_string();
    assert!(err.starts_with("QUESTION_NOT_PENDING"), "{err}");
    let p = pending_blocking();
    let Admission::New(mut r) = admit(&paths, "s", Some(&p), "call_x", &answers).unwrap() else {
        panic!("expected a new admission")
    };
    r.state = "answered".into();
    save_receipt(&paths, &mut r).unwrap();
    // The identical repeat returns the original receipt, even when gone.
    assert!(
        matches!(admit(&paths, "s", None, "call_x", &answers).unwrap(), Admission::Existing(e) if e.state == "answered")
    );
    // A different answer is refused.
    let other = parse_answers(r#"{"color":{"option_ids":["o2"]},"name":{"text":"a"}}"#).unwrap();
    let err = admit(&paths, "s", Some(&p), "call_x", &other)
        .err()
        .unwrap()
        .to_string();
    assert!(err.starts_with("QUESTION_ALREADY_ANSWERED"), "{err}");
    // An answered question is no longer projected as pending.
    assert!(project(&paths, "s", vec![p.clone()]).is_empty());
    // A definite rejection (nothing sent) leaves the question pending.
    r.state = "rejected".into();
    save_receipt(&paths, &mut r).unwrap();
    assert_eq!(project(&paths, "s", vec![p]).len(), 1);
    let events = fs::read_to_string(paths.root.join("questions/events.jsonl")).unwrap();
    assert_eq!(events.lines().count(), 2);
}

/// A fake Codex app-server that replays the pending request on `thread/resume`.
async fn fake_server(
    socket: PathBuf,
    replay: bool,
    seen: Arc<Mutex<Vec<Value>>>,
) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let v: Value = serde_json::from_str(text.as_str()).unwrap();
            seen.lock().unwrap().push(v.clone());
            let reply = |result: Value| {
                Message::Text(json!({"id":v["id"],"result":result}).to_string().into())
            };
            match v["method"].as_str() {
                Some("initialize") => ws.send(reply(json!({"userAgent":"codex_cli_rs/0.165.0 (linux)"}))).await.unwrap(),
                Some("thread/read") => ws.send(reply(json!({"thread":{"id":"thread-1","status":{"type":"active","activeFlags":["waitingOnUserInput"]}}}))).await.unwrap(),
                Some("thread/resume") => {
                    ws.send(reply(json!({"thread":{"id":"thread-1"}}))).await.unwrap();
                    if replay {
                        let request = json!({"method":"item/tool/requestUserInput","id":7,"params":{"threadId":"thread-1","turnId":"t","itemId":"call_x",
                            "questions":[{"id":"color","header":"Color","question":"Pick a color","isOther":true,"isSecret":false,"options":[{"label":"Red","description":"r"},{"label":"Blue","description":"b"}]},
                            {"id":"name","header":"Name","question":"Type any word","isOther":true,"isSecret":false,"options":[{"label":"Alpha","description":"a"},{"label":"Beta","description":"b"}]}],"isBlocking":true}});
                        ws.send(Message::Text(request.to_string().into())).await.unwrap();
                    }
                }
                None if v["id"] == 7 => {
                    let resolved = json!({"method":"serverRequest/resolved","params":{"threadId":"thread-1","requestId":7}});
                    ws.send(Message::Text(resolved.to_string().into())).await.unwrap();
                }
                _ => {}
            }
        }
    })
}

#[tokio::test]
async fn fake_app_server_question_round_trip_answers_once() {
    let (dir, paths) = fixture();
    let socket = dir.path().join("control.sock");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = fake_server(socket.clone(), true, seen.clone()).await;
    let p = pending_blocking();
    let answers =
        parse_answers(r#"{"color":{"option_ids":["o2"]},"name":{"text":"zebra"}}"#).unwrap();
    let Admission::New(mut r) = admit(&paths, "s", Some(&p), "call_x", &answers).unwrap() else {
        panic!()
    };
    codex_blocking(&paths, &socket, "thread-1", &p, &answers, &mut r)
        .await
        .unwrap();
    assert_eq!(r.state, "answered");
    assert_eq!(r.native_request_id, Some(json!(7)));
    let frames = seen.lock().unwrap().clone();
    let methods: Vec<_> = frames
        .iter()
        .map(|f| f["method"].as_str().unwrap_or("<response>"))
        .collect();
    assert_eq!(
        methods,
        [
            "initialize",
            "initialized",
            "thread/read",
            "thread/resume",
            "<response>"
        ]
    );
    assert_eq!(
        frames[3]["params"],
        json!({"threadId":"thread-1","excludeTurns":true})
    );
    assert_eq!(
        frames[4],
        json!({"id":7,"result":{"answers":{"color":{"answers":["Blue"]},"name":{"answers":["user_note: zebra"]}}}})
    );
    server.abort();
    // A second identical call is answered from the receipt without any connection.
    assert!(
        matches!(admit(&paths, "s", None, "call_x", &answers).unwrap(), Admission::Existing(e) if e.state == "answered")
    );
    assert_eq!(
        read_receipt(&paths, "s", "call_x").unwrap().state,
        "answered"
    );
}

#[tokio::test]
async fn a_request_the_server_no_longer_has_is_rejected_without_sending() {
    let (dir, paths) = fixture();
    let socket = dir.path().join("control.sock");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _server = fake_server(socket.clone(), false, seen.clone()).await;
    let p = pending_blocking();
    let answers =
        parse_answers(r#"{"color":{"option_ids":["o1"]},"name":{"option_ids":["o1"]}}"#).unwrap();
    let Admission::New(mut r) = admit(&paths, "s", Some(&p), "call_x", &answers).unwrap() else {
        panic!()
    };
    let err = codex_blocking(&paths, &socket, "thread-1", &p, &answers, &mut r)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "QUESTION_NOT_PENDING");
    assert_eq!(r.state, "rejected");
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .all(|f| f.get("result").is_none())
    );
    // Still listed: nothing was sent.
    assert_eq!(project(&paths, "s", vec![p]).len(), 1);
}

/// Opt-in check against a real Codex app-server and a THROWAWAY thread that is
/// waiting on `request_user_input` (never an operator conversation). Set
/// SESSIONSATION_LIVE_QUESTION_{SOCKET,THREAD,ROLLOUT}.
#[tokio::test]
#[ignore]
async fn live_throwaway_codex_question_round_trip() {
    let var = |k: &str| std::env::var(format!("SESSIONSATION_LIVE_QUESTION_{k}")).unwrap();
    let (_dir, paths) = fixture();
    let rollout = PathBuf::from(var("ROLLOUT"));
    let open = scan(&paths, Format::Codex, &rollout).unwrap();
    assert_eq!(open.len(), 1, "{open:?}");
    let p = open[0].clone();
    assert_eq!(p.kind, KIND_CODEX_BLOCKING);
    let answers =
        parse_answers(r#"{"color":{"option_ids":["o2"]},"name":{"text":"zebra"}}"#).unwrap();
    let Admission::New(mut r) = admit(&paths, "s", Some(&p), &p.request_id, &answers).unwrap()
    else {
        panic!()
    };
    codex_blocking(
        &paths,
        Path::new(&var("SOCKET")),
        &var("THREAD"),
        &p,
        &answers,
        &mut r,
    )
    .await
    .unwrap();
    assert_eq!(r.state, "answered");
    // Double answer: refused locally, nothing reaches the provider.
    let other = parse_answers(r#"{"color":{"option_ids":["o1"]},"name":{"text":"x"}}"#).unwrap();
    assert!(admit(&paths, "s", Some(&p), &p.request_id, &other).is_err());
    // The output record ends the question in the rollout.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(scan(&paths, Format::Codex, &rollout).unwrap().is_empty());
    println!(
        "LIVE_OK {}",
        serde_json::to_string(&public_receipt(&r, None)).unwrap()
    );
}
