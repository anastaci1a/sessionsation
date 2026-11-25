use super::*;
use crate::messaging::tests::{attachment_fixture, bind, fixture};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;
async fn scripted_server(
    path: &std::path::Path,
    handler: impl Fn(&Value) -> Option<Value> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                if req["method"] == "initialized" {
                    continue;
                }
                let result = if req["method"] == "initialize" {
                    Some(json!({"userAgent":"test-originator/0.163.0 (Linux)"}))
                } else {
                    handler(&req)
                };
                let Some(result) = result else {
                    break;
                };
                if socket
                    .send(Message::Text(if result["fixture_error"].is_object() {
                        json!({"id":req["id"],"error":result["fixture_error"]})
                            .to_string()
                            .into()
                    } else {
                        json!({"id":req["id"],"result":result}).to_string().into()
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    })
}
/// Full-view turn in the live-verified 0.165 shape (synthetic text).
fn full_turn(id: &str, status: &str, steer: Option<&str>) -> Value {
    let mut items = vec![
        json!({"type":"userMessage","id":"u-0","clientId":null,"content":[{"type":"text","text":"PRIVATE prompt","text_elements":[]}]}),
        json!({"type":"reasoning","id":"r-0","summary":[],"content":["PRIVATE reasoning"]}),
        json!({"type":"agentMessage","id":"msg_before","text":"working","phase":"commentary","memoryCitation":null,"delivery":null,"questions":null}),
    ];
    if let Some(client) = steer {
        items.push(json!({"type":"userMessage","id":"u-steer","clientId":client,"content":[{"type":"text","text":"PRIVATE steer","text_elements":[]}]}));
        items.push(json!({"type":"agentMessage","id":"msg_after","text":"done STEERED","phase":"final_answer","memoryCitation":null,"delivery":null,"questions":null}));
    }
    json!({"id":id,"status":status,"itemsView":"full","items":items,"error":null,"startedAt":1,"completedAt":null,"durationMs":null,"rootTurnId":null})
}
#[tokio::test]
async fn explicit_steering_is_exact_turn_correlated_and_never_replayed() {
    for host in ["%1", "herdr/fixture/w1:p1"] {
        for scenario in [
            "success",
            "compact",
            "review",
            "idle",
            "turn_changed",
            "runtime_changed",
            "catalog_changed",
            "lost_ack",
            "wrong_ack",
            "stopped",
            "migration",
            "restart_after_intent",
            "restart_before_intent",
            "runtime_race",
        ] {
            let (_tmp, paths, mut input, _) = fixture();
            input.expires_at = crate::daemon::now() + 120;
            let mut a = attachment_fixture(&paths, &input);
            bind(&paths, &mut a);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let seen = calls.clone();
            let native = a.id.clone();
            let client = input.message_id.clone();
            let binding_path = paths.root.join(crate::layout::MANAGED_PANES).join("1.json");
            let steered = Arc::new(Mutex::new(false));
            let server=scripted_server(&a.endpoint(),move |req| {
                let method=req["method"].as_str().unwrap(); seen.lock().unwrap().push(if req["params"]["itemsView"]=="full" {"full".to_owned()} else {method.to_owned()});
                if method == "thread/turns/list" && req["params"]["itemsView"]=="full" {
                    // Live 0.165 item shapes with synthetic text.
                    let recorded = *steered.lock().unwrap() && scenario != "wrong_ack";
                    return Some(json!({"data":[full_turn(if scenario=="turn_changed"{"turn-other"}else{"turn-active"}, if recorded {"completed"} else {"inProgress"}, recorded.then_some(client.as_str()))],"nextCursor":null}));
                }
                if method == "thread/turns/list" && scenario == "runtime_race" { let _ = fs::remove_file(&binding_path); }
                match method {
                    "thread/read"=>Some(json!({"thread":{"id":native,"status":{"type":if scenario=="idle"{"idle"}else{"active"}}}})),
                    "thread/turns/list"=>Some(json!({"data":[{"id":if scenario=="turn_changed"{"turn-other"}else{"turn-active"},"status":"inProgress"}]})),
                    "turn/steer"=>{
                        assert_eq!(req["params"]["expectedTurnId"],"turn-active");
                        assert_eq!(req["params"]["clientUserMessageId"],client);
                        assert_eq!(req["params"]["threadId"],native);
                        *steered.lock().unwrap() = !matches!(scenario, "compact" | "review");
                        if matches!(scenario,"compact"|"review"){Some(json!({"fixture_error":{"code":-32600,"message":"PRIVATE native diagnostic","data":{"codexErrorInfo":{"activeTurnNotSteerable":{"turnKind":scenario}}}}}))}else if scenario=="lost_ack"{None}else{Some(json!({"turnId":if scenario=="wrong_ack"{"turn-other"}else{"turn-active"}}))}
                    },
                    _=>panic!("Forbidden RPC {method}"),
                }
            }).await;
            let source:external::Instance=serde_json::from_value(json!({"pane":host,"tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
            let s = crate::catalog::reconcile(&paths, &[source])
                .unwrap()
                .remove(0);
            let request = PublicRequest {
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                api_version: 1,
                message_id: input.message_id.clone(),
                session_id: s.id.clone(),
                expected_generation: s.generation(),
                mode: PublicMode::Steer,
                text: input.text.clone(),
                expires_at: input.expires_at,
                recovery: None,
                steering: Some(Intent {
                    version: 1,
                    expected_turn_id: "turn-active".into(),
                    expected_runtime_generation: if scenario == "runtime_changed" {
                        "0".repeat(64)
                    } else {
                        a.generation()
                    },
                }),
            };
            if scenario == "stopped" {
                fs::create_dir_all(paths.root.join(crate::layout::CONVERSATIONS)).unwrap();
                fs::write(
                    paths
                        .root
                        .join(crate::layout::CONVERSATIONS)
                        .join(format!("{}.json", a.id)),
                    json!({"stop_intent":"stop-1"}).to_string(),
                )
                .unwrap();
            }
            let migration = if scenario == "migration" {
                Some(mutation_lock(&paths, &a.id).unwrap())
            } else {
                None
            };
            if scenario == "catalog_changed" {
                crate::catalog::rename(&paths, &s.id, &s.generation(), "changed").unwrap();
            }
            if matches!(scenario, "restart_after_intent" | "restart_before_intent") {
                let mut db = Ledger::open(&paths).unwrap();
                let b = Binding {
                    cancelled: false,
                    released_hold: false,
                    origin: None,
                    origin_source: None,
                    evidence_observed_at: None,
                    plan: None,
                    request: request.clone(),
                    native: Enqueue {
                        expected_generation: a.generation(),
                        ..input.clone()
                    },
                };
                db.db
                    .execute(
                        "INSERT INTO public_messages VALUES (?,?)",
                        params![request.message_id, serde_json::to_string(&b).unwrap()],
                    )
                    .unwrap();
                assert_eq!(
                    db.accept(&b.native, crate::daemon::now()).unwrap_err().0,
                    "message_id_conflict",
                    "legacy must not adopt a bound public intent"
                );
                if scenario == "restart_after_intent" {
                    let mut r = accept(&mut db, &b).unwrap();
                    r.state = "unknown".into();
                    r.submit = Some(SubmitEvidence {
                        expected_turn_id: "turn-active".into(),
                        acknowledged_turn_id: None,
                        action: Some("public_steer".into()),
                        resume: false,
                    });
                    db.save(&r).unwrap();
                }
            }
            if matches!(scenario, "success" | "idle") {
                let hint = target(&paths, &s.id, &s.generation()).await.unwrap();
                assert_eq!(
                    hint["available"],
                    scenario == "success",
                    "{host} {scenario}: {hint}"
                );
                assert_eq!(hint["sends_input"], false);
                if scenario == "success" {
                    assert_eq!(hint["active_turn_id"], "turn-active");
                }
            }
            let first = send_public(&paths, request.clone()).await;
            if scenario == "catalog_changed" {
                assert!(first.unwrap_err().to_string().contains("STALE_TARGET"));
                server.abort();
                continue;
            }
            let first = first.unwrap();
            assert_eq!(first["delivery_semantics"], "deliberate_in_turn_steer");
            assert_eq!(
                first["facts"]["accepted_native"],
                matches!(scenario, "success" | "restart_before_intent"),
                "{host} {scenario}: {first}"
            );
            assert_eq!(first["facts"]["input_recorded"], false);
            assert_eq!(first["reply_correlation"], "after_steer_item");
            assert_eq!(first["acceptance_only"], false);
            assert!(first["reply"].is_null() && first["correlation"].is_null());
            assert_eq!(
                first["state"],
                match scenario {
                    "success" | "restart_before_intent" => "native_accepted",
                    "lost_ack" | "wrong_ack" | "restart_after_intent" => "delivery_unknown",
                    _ => "rejected",
                }
            );
            if matches!(scenario, "compact" | "review") {
                assert_eq!(first["reason"], format!("{scenario}_turn_not_steerable"));
                assert_eq!(first["facts"]["native_acceptance"], "not_attempted");
                assert!(!first.to_string().contains("PRIVATE"));
            }
            let count = calls.lock().unwrap().len();
            let read = receipt_public(&paths, &request.message_id, false)
                .await
                .unwrap();
            let correlated = matches!(scenario, "success" | "restart_before_intent" | "lost_ack");
            if correlated {
                // The steer's own input item anchors the reply, even after a lost ack.
                assert_eq!(read["state"], "completed_with_reply", "{scenario}: {read}");
                assert_eq!(read["facts"]["input_recorded"], true);
                assert_eq!(read["facts"]["accepted_native"], true);
                assert_eq!(
                    read["facts"]["native_acceptance"],
                    if scenario == "lost_ack" {
                        "input_observed"
                    } else {
                        "accepted"
                    }
                );
                assert_eq!(read["facts"]["turn_completed"], true);
                assert_eq!(read["facts"]["turn_status"], "completed");
                assert_eq!(read["facts"]["shared_with_turn_tail"], true);
                assert_eq!(read["reply"]["item_id"], "msg_after");
                assert_eq!(read["reply"]["text"], "done STEERED");
                assert_eq!(read["reply"]["final"], true);
                assert_eq!(read["correlation"]["steer_input_item_id"], "u-steer");
                assert_eq!(read["observation"]["state"], "observed");
                assert!(!read.to_string().contains("PRIVATE"));
            } else {
                assert_eq!(read["state"], first["state"]);
                assert!(read["reply"].is_null());
            }
            let reads: Vec<String> = calls.lock().unwrap()[count..].to_vec();
            let observable = correlated || matches!(scenario, "wrong_ack" | "restart_after_intent");
            assert!(
                reads.iter().all(|m| m == "full") && reads.is_empty() != observable,
                "{scenario}: receipt reads only the steered turn: {reads:?}"
            );
            let count = calls.lock().unwrap().len();
            let cached = receipt_public(&paths, &request.message_id, false)
                .await
                .unwrap();
            assert_eq!(cached["state"], read["state"]);
            if correlated {
                assert_eq!(cached["observation"]["state"], "cached");
                assert_eq!(
                    calls.lock().unwrap().len(),
                    count,
                    "terminal evidence is cached"
                );
            }
            send_public(&paths, request.clone()).await.unwrap();
            let mut conflict = request.clone();
            conflict.steering.as_mut().unwrap().expected_turn_id = "changed".into();
            assert!(
                send_public(&paths, conflict)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("MESSAGE_ID_CONFLICT")
            );
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| m.as_str() == "turn/steer")
                    .count(),
                usize::from(matches!(
                    scenario,
                    "success"
                        | "restart_before_intent"
                        | "lost_ack"
                        | "wrong_ack"
                        | "compact"
                        | "review"
                ))
            );
            if matches!(
                scenario,
                "success" | "lost_ack" | "wrong_ack" | "restart_after_intent"
            ) {
                assert!(
                    receipt_public(&paths, &request.message_id, true)
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("STEERING_CANCELLATION_UNSUPPORTED")
                );
            }
            drop(migration);
            server.abort();
        }
    }
}
fn user(id: &str, client: Option<&str>) -> Value {
    json!({"type":"userMessage","id":id,"clientId":client,"content":[{"type":"text","text":"PRIVATE","text_elements":[]}]})
}
fn agent(id: &str, text: &str, phase: &str) -> Value {
    json!({"type":"agentMessage","id":id,"text":text,"phase":phase,"memoryCitation":null,"delivery":null,"questions":null})
}
fn turn(id: &str, status: &str, items: Vec<Value>) -> Value {
    json!({"id":id,"status":status,"itemsView":"full","items":items})
}
/// Full-view responses per observation phase: 0 = before steer (anchor),
/// 1 = first receipt read, 2 = second receipt read.
fn script(case: &str, phase: u32, client: &str, cursor: &Value) -> Value {
    let base = || {
        vec![
            user("u-0", None),
            agent("msg_before", "working", "commentary"),
        ]
    };
    let with = |mut extra: Vec<Value>| {
        let mut items = base();
        items.append(&mut extra);
        items
    };
    let page = |t: Value| json!({"data":[t],"nextCursor":null});
    match (case, phase) {
        ("anchor_mismatch", _) => page(turn("turn-next", "inProgress", base())),
        (_, 0) => page(turn("turn-active", "inProgress", base())),
        ("interrupted", n) => page(turn(
            "turn-active",
            if n == 1 { "inProgress" } else { "interrupted" },
            with(vec![
                user("u-steer", Some(client)),
                agent("msg_p", "partial", "commentary"),
            ]),
        )),
        ("not_consumed", n) => page(turn(
            "turn-active",
            if n == 1 { "inProgress" } else { "completed" },
            base(),
        )),
        ("two_steers", _) => page(turn(
            "turn-active",
            "completed",
            with(vec![
                user("u-steer", Some(client)),
                agent("msg_mine", "mine", "final_answer"),
                user("u-later", Some("33333333-3333-4333-8333-333333333333")),
                agent("msg_tail", "tail", "final_answer"),
            ]),
        )),
        ("other_turn", _) if cursor.is_null() => {
            json!({"data":[turn("turn-new","inProgress",vec![user("u-steer", Some(client))])],"nextCursor":"c1"})
        }
        ("other_turn", _) => page(turn("turn-active", "completed", base())),
        _ => unreachable!(),
    }
}
#[tokio::test]
async fn steer_reply_progresses_and_classifies_terminal_outcomes() {
    for case in [
        "interrupted",
        "not_consumed",
        "two_steers",
        "anchor_mismatch",
        "other_turn",
    ] {
        let (_tmp, paths, mut input, _) = fixture();
        input.expires_at = crate::daemon::now() + 120;
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let native = a.id.clone();
        let client = input.message_id.clone();
        let phase = Arc::new(Mutex::new(0u32));
        let steers = Arc::new(Mutex::new(0u32));
        let (p, n) = (phase.clone(), steers.clone());
        let server = scripted_server(&a.endpoint(), move |req| {
            let full = req["params"]["itemsView"] == "full";
            match req["method"].as_str().unwrap() {
                "thread/read" => Some(json!({"thread":{"id":native,"status":{"type":"active"}}})),
                "thread/turns/list" if full => Some(script(
                    case,
                    *p.lock().unwrap(),
                    &client,
                    &req["params"]["cursor"],
                )),
                "thread/turns/list" => {
                    Some(json!({"data":[{"id":"turn-active","status":"inProgress"}]}))
                }
                "turn/steer" => {
                    *n.lock().unwrap() += 1;
                    Some(json!({"turnId":"turn-active"}))
                }
                m => panic!("Forbidden RPC {m}"),
            }
        })
        .await;
        let source:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let s = crate::catalog::reconcile(&paths, &[source])
            .unwrap()
            .remove(0);
        let request = PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: None,
            api_version: 1,
            message_id: input.message_id.clone(),
            session_id: s.id.clone(),
            expected_generation: s.generation(),
            mode: PublicMode::Steer,
            text: input.text.clone(),
            expires_at: input.expires_at,
            recovery: None,
            steering: Some(Intent {
                version: 1,
                expected_turn_id: "turn-active".into(),
                expected_runtime_generation: a.generation(),
            }),
        };
        let first = send_public(&paths, request.clone()).await.unwrap();
        if case == "anchor_mismatch" {
            assert_eq!(first["state"], "rejected", "{first}");
            assert_eq!(first["reason"], "active_turn_changed");
            assert_eq!(*steers.lock().unwrap(), 0, "no steer without an anchor");
            server.abort();
            continue;
        }
        assert_eq!(first["state"], "native_accepted");
        *phase.lock().unwrap() = 1;
        let id = &request.message_id;
        let r1 = receipt_public(&paths, id, false).await.unwrap();
        *phase.lock().unwrap() = 2;
        let r2 = receipt_public(&paths, id, false).await.unwrap();
        assert!(!r2.to_string().contains("PRIVATE"));
        match case {
            "interrupted" => {
                assert_eq!(r1["state"], "execution_started", "{r1}");
                assert_eq!(r1["reply"]["text"], "partial");
                assert_eq!(r1["reply"]["final"], false);
                assert_eq!(r1["facts"]["input_consumed"], true);
                assert_eq!(r1["facts"]["turn_completed"], false);
                assert_eq!(r1["facts"]["turn_status"], "inProgress");
                assert_eq!(
                    r1["correlation"]["anchor"]["last_agent_item_id"],
                    "msg_before"
                );
                assert_eq!(r1["correlation"]["anchor"]["user_items_before"], 1);
                assert_eq!(r2["state"], "failed");
                assert_eq!(r2["reason"], "steer_turn_interrupted");
                assert_eq!(r2["reply"]["text"], "partial");
                assert_eq!(r2["reply"]["final"], false);
                assert_eq!(r2["facts"]["turn_status"], "interrupted");
            }
            "not_consumed" => {
                assert_eq!(r1["state"], "native_accepted");
                assert_eq!(r1["observation"]["state"], "uncorrelated");
                assert_eq!(r2["state"], "failed", "{r2}");
                assert_eq!(r2["reason"], "input_not_consumed");
                assert_eq!(r2["facts"]["input_recorded"], false);
                assert_eq!(r2["facts"]["input_consumed"], false);
                assert!(r2["reply"].is_null() && r2["correlation"].is_null());
            }
            "two_steers" => {
                assert_eq!(r1["state"], "completed_with_reply", "{r1}");
                assert_eq!(r1["reply"]["item_id"], "msg_mine");
                assert_eq!(r1["reply"]["shared_with_turn_tail"], false);
                assert_eq!(r1["correlation"]["next_input_item_id"], "u-later");
                assert_eq!(r2["observation"]["state"], "cached");
            }
            "other_turn" => {
                assert_eq!(r1["state"], "native_accepted", "{r1}");
                assert_eq!(r1["observation"]["error_code"], "correlation_changed");
                assert!(r1["reply"].is_null());
            }
            _ => unreachable!(),
        }
        assert_eq!(*steers.lock().unwrap(), 1, "observation never resends");
        server.abort();
    }
}

/// A steer that overlaps another send waits for session ownership and the
/// ledger instead of being rejected `operation_busy`, runs its native I/O
/// without the ledger lease, and two processes never steer one id twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steer_waits_releases_the_lease_and_never_steers_twice() {
    let (_tmp, paths, mut input, _) = fixture();
    input.expires_at = crate::daemon::now() + 120;
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    crate::messaging::TEST_WAIT
        .lock()
        .unwrap()
        .push((paths.root.clone(), Duration::from_secs(10)));
    let steers = Arc::new(Mutex::new(0usize));
    let lease_free = Arc::new(Mutex::new(None::<String>));
    let (counter, probe, p) = (steers.clone(), lease_free.clone(), paths.clone());
    let native = a.id.clone();
    let client = input.message_id.clone();
    let steered = Arc::new(Mutex::new(false));
    let server = scripted_server(&a.endpoint(), move |req| {
        let method = req["method"].as_str().unwrap();
        if method == "thread/turns/list" && req["params"]["itemsView"] == "full" {
            let recorded = *steered.lock().unwrap();
            return Some(json!({"data":[full_turn("turn-active", "inProgress", recorded.then_some(client.as_str()))],"nextCursor":null}));
        }
        match method {
            "thread/read" => Some(json!({"thread":{"id":native,"status":{"type":"active"}}})),
            "thread/turns/list" => Some(json!({"data":[{"id":"turn-active","status":"inProgress"}]})),
            "turn/steer" => {
                *counter.lock().unwrap() += 1;
                *probe.lock().unwrap() = Ledger::open(&p)
                    .ok()
                    .and_then(|db| db.get(&client).ok().flatten())
                    .map(|r| r.state);
                std::thread::sleep(Duration::from_millis(500));
                *steered.lock().unwrap() = true;
                Some(json!({"turnId":"turn-active"}))
            }
            _ => panic!("Forbidden RPC {method}"),
        }
    })
    .await;
    let source: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
    let s = crate::catalog::reconcile(&paths, &[source])
        .unwrap()
        .remove(0);
    let request = PublicRequest {
        origin: None,
        delivery: None,
        await_reply: false,
        if_busy: None,
        api_version: 1,
        message_id: input.message_id.clone(),
        session_id: s.id.clone(),
        expected_generation: s.generation(),
        mode: PublicMode::Steer,
        text: input.text.clone(),
        expires_at: input.expires_at,
        recovery: None,
        steering: Some(Intent {
            version: 1,
            expected_turn_id: "turn-active".into(),
            expected_runtime_generation: a.generation(),
        }),
    };
    let binding = Binding {
        cancelled: false,
        released_hold: false,
        origin: None,
        origin_source: None,
        evidence_observed_at: None,
        plan: None,
        request: request.clone(),
        native: Enqueue {
            expected_generation: a.generation(),
            ..input.clone()
        },
    };
    {
        let db = Ledger::open(&paths).unwrap();
        db.db
            .execute(
                "INSERT INTO public_messages VALUES (?,?)",
                params![request.message_id, serde_json::to_string(&binding).unwrap()],
            )
            .unwrap();
    }
    let binding = Arc::new(binding);
    // Another send holds the session briefly: the steer waits, then proceeds.
    let held = mutation_lock(&paths, &a.id).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(held);
    });
    let run = |paths: Paths, binding: Arc<Binding>| {
        tokio::spawn(async move {
            let mut db = ledger::open_write(&paths, Duration::from_secs(10))
                .await
                .unwrap();
            let mut r = accept(&mut db, &binding).unwrap();
            let outcome = deliver(&paths, &binding, &mut db, &mut r).await;
            (r, outcome.map_err(|e| e.to_string()))
        })
    };
    let first = run(paths.clone(), binding.clone());
    let second = run(paths.clone(), binding.clone());
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    release.join().unwrap();
    // One call steers. The other either adopts that result or, finding its own
    // input already recorded in the turn, refuses; it never waits out as busy.
    let mut accepted = 0;
    for (r, outcome) in [&first, &second] {
        match outcome {
            Ok(()) => {
                assert_eq!(r.state, "native_accepted");
                accepted += 1;
            }
            Err(e) => assert_ne!(e, "operation_busy"),
        }
    }
    assert!(accepted >= 1);
    let stored = Ledger::open(&paths)
        .unwrap()
        .get(&input.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, "native_accepted");
    assert_eq!(*steers.lock().unwrap(), 1, "one id is never steered twice");
    // The ledger lease was free, with the journal durable, during the native steer.
    assert_eq!(lease_free.lock().unwrap().as_deref(), Some("unknown"));
    server.abort();
}
