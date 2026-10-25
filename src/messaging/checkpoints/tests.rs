use super::*;
use crate::messaging::tests::{attachment_fixture, bind, fixture};
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

fn page(turn: &str, text: &str) -> Page {
    serde_json::from_value(json!({"data":[
        {"turnId":turn,"item":{"type":"reasoning","text":"PRIVATE_REASONING"}},
        {"turnId":turn,"item":{"type":"commandExecution","output":"PRIVATE_TOOL"}},
        {"turnId":turn,"item":{"type":"userMessage","text":"PRIVATE_INPUT"}},
        {"turnId":turn,"item":{"type":"agentMessage","id":"public-1","phase":"commentary","text":text}}
    ],"nextCursor":null})).unwrap()
}
#[test]
fn durable_order_dedup_revisions_gaps_and_private_projection() {
    let mut j = Journal {
        epoch: "epoch".into(),
        ..Default::default()
    };
    j.ingest("scope", "turn", page("turn", "Checkpoint"), 1)
        .unwrap();
    let first = j.events.clone();
    let bytes = serde_json::to_vec(&j).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE"));
    let mut j: Journal = serde_json::from_slice(&bytes).unwrap();
    j.ingest("scope", "turn", page("turn", "Checkpoint"), 2)
        .unwrap();
    assert_eq!(j.events, first);
    j.ingest("scope", "turn", page("turn", "Checkpoint continued"), 3)
        .unwrap();
    assert_eq!(j.sequence, 2);
    assert_ne!(j.events[0]["event_id"], j.events[1]["event_id"]);
    assert_eq!(j.position(Some("epoch:1")).unwrap(), 1);
    assert!(j.position(Some("other:1")).is_err());
    for n in 0..260 {
        j.ingest("scope", "turn", page("turn", &format!("Update {n}")), 4)
            .unwrap();
    }
    assert_eq!(j.events.len(), 256);
    assert!(j.position(Some("epoch:1")).is_err());
    assert!(j.position(Some("epoch:9999")).is_err());
    assert!(
        j.ingest("scope", "turn", page("different", "Wrong"), 5)
            .is_err()
    );
}
#[test]
fn invalid_pages_and_capacity_fail_closed() {
    let mut j = Journal::default();
    let mut p = page("t", "x");
    p.next = Some("cursor".into());
    j.next = Some("cursor".into());
    assert!(j.ingest("s", "t", p, 1).is_err());
    j.next = None;
    j.seen = (0..4096).map(|n| n.to_string()).collect();
    assert_eq!(
        j.ingest("s", "t", page("t", "x"), 1).unwrap_err().0,
        "checkpoint_retention_limit"
    );
}
#[tokio::test]
async fn exact_active_turn_checkpoints_continue_and_survive_restart_for_both_hosts() {
    for (host, history_mode) in [
        ("%1", "paginated"),
        ("herdr/fixture/w1:p1", "paginated"),
        ("%1", "legacy"),
        ("herdr/fixture/w1:p1", "legacy"),
    ] {
        let (_tmp, paths, input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let source:external::Instance=serde_json::from_value(json!({"pane":host,"tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let s = crate::catalog::reconcile(&paths, &[source])
            .unwrap()
            .remove(0);
        let mode = Arc::new(Mutex::new("first"));
        let server_mode = mode.clone();
        let listener = UnixListener::bind(a.endpoint()).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    let m = req["method"].as_str().unwrap();
                    if m == "initialized" {
                        continue;
                    }
                    seen.lock().unwrap().push(m.to_string());
                    let mode = *server_mode.lock().unwrap();
                    let result = match m {
                        "initialize" => json!({"userAgent":"fixture/0.164.0 (Linux)"}),
                        "thread/read" => {
                            json!({"thread":{"id":req["params"]["threadId"],"historyMode":history_mode}})
                        }
                        "thread/turns/list" if req["params"]["itemsView"] == "full" => {
                            assert_eq!(history_mode, "legacy");
                            if mode == "disconnect" {
                                break;
                            }
                            json!({"data":[{"id":if mode=="wrong"{"other"}else{"tracked"},"status":if mode=="failed"{"failed"}else{"inProgress"},"itemsView":"full","items":[{"type":"commandExecution","output":"PRIVATE_TOOL".repeat(100000)},{"type":"agentMessage","id":"public","phase":"commentary","text":if mode=="first"{"Checkpoint while working"}else{"Still working"}}]}]})
                        }
                        "thread/turns/list" => {
                            json!({"data":[{"id":if mode=="stale"{"other"}else{"tracked"},"status":if mode=="failed"{"failed"}else{"inProgress"}}]})
                        }
                        "thread/items/list" => {
                            assert_eq!(
                                history_mode, "paginated",
                                "legacy does not support this RPC"
                            );
                            assert_eq!(req["params"]["turnId"], "tracked");
                            assert_eq!(req["params"]["limit"], 50);
                            if mode == "disconnect" {
                                break;
                            }
                            json!({"data":[{"turnId":if mode=="wrong"{"other"}else{"tracked"},"item":{"type":"agentMessage","id":"public","phase":"commentary","text":if mode=="first"{"Checkpoint while working"}else{"Still working"}}}],"nextCursor":null})
                        }
                        _ => panic!("Forbidden RPC {m}"),
                    };
                    if ws
                        .send(Message::Text(
                            json!({"id":req["id"],"result":result}).to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let request = |cursor: Option<String>| Request {
            version: 1,
            session_id: s.id.clone(),
            expected_generation: s.generation(),
            expected_runtime_generation: a.generation(),
            expected_turn_id: "tracked".into(),
            cursor,
        };
        let first = read(&paths, request(None)).await.unwrap();
        assert_eq!(first["turn_status"], "inProgress");
        assert_eq!(first["events"][0]["text"], "Checkpoint while working");
        assert_eq!(first["delivery_evidence"], false);
        let cursor = first["cursor"].as_str().unwrap().to_string();
        // A new invocation reloads durable journal, not in-memory observer state.
        let item_reads = || {
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == "thread/items/list")
                .count()
        };
        let before_replay = item_reads();
        let replay = read(&paths, request(None)).await.unwrap();
        assert_eq!(replay["events"], first["events"]);
        assert_eq!(replay["native_page_read"], false);
        assert_eq!(item_reads(), before_replay);
        assert_eq!(
            read(&paths, request(Some(cursor.clone()))).await.unwrap()["events"],
            json!([])
        );
        *mode.lock().unwrap() = "continued";
        let continued = read(&paths, request(Some(cursor.clone()))).await.unwrap();
        assert_eq!(continued["events"][0]["sequence"], 2);
        let cursor = continued["cursor"].as_str().unwrap().to_string();
        for (m, expected) in [
            ("stale", "checkpoint_stale_turn"),
            (
                "wrong",
                if history_mode == "legacy" {
                    "checkpoint_stale_turn"
                } else {
                    "checkpoint_turn_mismatch"
                },
            ),
            ("disconnect", "disconnected"),
        ] {
            *mode.lock().unwrap() = m;
            assert!(
                read(&paths, request(Some(cursor.clone())))
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
        *mode.lock().unwrap() = "failed";
        assert_eq!(
            read(&paths, request(Some(cursor))).await.unwrap()["turn_status"],
            "failed"
        );
        let mut wrong = request(None);
        wrong.expected_runtime_generation = "wrong".into();
        assert!(
            read(&paths, wrong)
                .await
                .unwrap_err()
                .to_string()
                .contains("STALE_TARGET")
        );
        assert!(calls.lock().unwrap().iter().all(|m| matches!(
            m.as_str(),
            "initialize" | "thread/read" | "thread/turns/list" | "thread/items/list"
        )));
        server.abort();
    }
}

#[test]
fn paginated_scan_retains_order_and_restarts_without_duplicates() {
    let mut j = Journal {
        epoch: "epoch".into(),
        ..Default::default()
    };
    let mut first = page("t", "first");
    first.next = Some("page2".into());
    j.ingest("s", "t", first, 1).unwrap();
    assert_eq!(j.next.as_deref(), Some("page2"));
    let mut second = page("t", "second");
    second.data.last_mut().unwrap().item = serde_json::value::to_raw_value(
        &json!({"type":"agentMessage","id":"public-2","phase":"partial_answer","text":"second"}),
    )
    .unwrap();
    j.ingest("s", "t", second, 2).unwrap();
    assert_eq!(j.next, None);
    j.ingest("s", "t", page("t", "first"), 3).unwrap();
    assert_eq!(j.events.len(), 2);
    assert_eq!(j.events[1]["sequence"], 2);
    let mut unknown = page("t", "unknown");
    unknown.data.last_mut().unwrap().item = serde_json::value::to_raw_value(
        &json!({"type":"agentMessage","id":"private","text":"NO_PHASE"}),
    )
    .unwrap();
    j.ingest("s", "t", unknown, 4).unwrap();
    assert_eq!(j.events.len(), 2);
}

#[test]
fn wire_byte_pages_preserve_whole_large_events_and_cursor_progress() {
    for text in ["x".repeat(65536), "\0".repeat(65536), "界".repeat(21845)] {
        let mut j = Journal {
            epoch: "epoch".into(),
            ..Default::default()
        };
        for n in 0..50 {
            let p: Page = serde_json::from_value(json!({"data":[{"turnId":"turn","item":{"type":"agentMessage","id":format!("public-{n}"),"phase":"commentary","text":text}}],"nextCursor":null})).unwrap();
            j.ingest("scope", "turn", p, 1).unwrap();
        }
        let fixture: Value =
            serde_json::from_str(include_str!("../../../docs/contract/fixtures.json")).unwrap();
        let mut position = 0;
        let mut sequences = Vec::new();
        loop {
            let result = j
                .response(position, fixture["checkpoints"].clone())
                .unwrap();
            let mut wire =
                serde_json::to_vec(&json!({"api_version":1,"ok":true,"result":result})).unwrap();
            wire.push(b'\n');
            assert!(wire.len() <= MAX_RESPONSE_BYTES);
            let events = result["events"].as_array().unwrap();
            assert!(!events.is_empty());
            for event in events {
                assert_eq!(event["text"], text);
                sequences.push(event["sequence"].as_u64().unwrap());
            }
            let next = j.position(result["cursor"].as_str()).unwrap();
            assert!(next > position);
            assert_eq!(
                j.response(position, fixture["checkpoints"].clone())
                    .unwrap(),
                result
            );
            position = next;
            if result["has_more"] == false {
                break;
            }
            assert_eq!(result["byte_limited"], true);
        }
        assert_eq!(sequences, (1..=50).collect::<Vec<_>>());
    }
    let mut j = Journal::default();
    assert_eq!(
        j.ingest("scope", "turn", page("turn", &"x".repeat(65537)), 1)
            .unwrap_err()
            .0,
        "checkpoint_item_limit"
    );
}

#[tokio::test]
async fn native_rejection_diagnostic_contains_only_method_and_code() {
    let (_tmp, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let listener = UnixListener::bind(a.endpoint()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let r: Value = serde_json::from_str(&text).unwrap();
            if r["method"] == "initialized" {
                continue;
            }
            let v = if r["method"] == "initialize" {
                json!({"id":r["id"],"result":{"userAgent":"fixture/0.164.0"}})
            } else {
                json!({"id":r["id"],"error":{"code":-32601,"message":"PRIVATE_NATIVE_DETAIL","data":{"secret":"PRIVATE_FRAME"}}})
            };
            ws.send(Message::Text(v.to_string().into())).await.unwrap();
        }
    });
    let mut c = a.connect_checkpoints().await.unwrap();
    assert_eq!(
        c.checkpoint_items::<Value>(&a.id, "t", None)
            .await
            .unwrap_err()
            .0,
        "native_rejected"
    );
    let d = c.last_rejection.as_ref().unwrap();
    assert_eq!(
        serde_json::to_value(d).unwrap(),
        json!({"method":"thread/items/list","code":-32601})
    );
    assert!(!d.to_string().contains("PRIVATE"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires SESSIONSATION_NATIVE_CHECKPOINT_BINARY; isolated native control server, no credentials/model calls"]
async fn installed_native_control_legacy_checkpoint_projection() {
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::Instant,
    };
    let binary = std::env::var("SESSIONSATION_NATIVE_CHECKPOINT_BINARY")
        .expect("explicit isolated native binary");
    let (_tmp, paths, input, _) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    fs::set_permissions(
        a.home.join("app-server-control"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let rollout = a.home.join(format!(
        "sessions/2026/10/24/rollout-2026-10-24T00-00-00-{}.jsonl",
        a.id
    ));
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let line = |kind: &str, payload: Value| {
        json!({"timestamp":"2026-10-24T00:00:00.000Z","type":kind,"payload":payload}).to_string()
            + "\n"
    };
    let text = line(
        "session_meta",
        json!({"id":a.id,"session_id":a.id,"timestamp":"2026-10-24T00:00:00Z","cwd":a.home,"originator":"isolated_checkpoint_fixture","cli_version":"0.164.0","source":"cli","model_provider":"openai","history_mode":"legacy"}),
    ) + &line(
        "event_msg",
        json!({"type":"task_started","turn_id":"fixture-turn"}),
    ) + &line(
        "event_msg",
        json!({"type":"agent_message","phase":"commentary","message":"Synthetic native checkpoint"}),
    );
    fs::write(&rollout, text).unwrap();
    fs::write(a.home.join("config.toml"), "[analytics]\nenabled = false\n").unwrap();
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Owned(
        Command::new(binary)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &a.home)
            .env("CODEX_HOME", &a.home)
            .env("TOKIO_WORKER_THREADS", "2")
            .args([
                "app-server",
                "--listen",
                &format!("unix://{}", a.endpoint().display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    while !a.endpoint().exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "isolated native server exited"
        );
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut c = a.connect_checkpoints().await.unwrap();
    assert_eq!(history_mode(&mut c, &a.id).await.unwrap(), "legacy");
    assert!(
        c.checkpoint_items::<Value>(&a.id, "fixture-turn", None)
            .await
            .is_err()
    );
    assert_eq!(
        c.last_rejection.as_ref().unwrap().code,
        -32601,
        "real legacy gate must reject the previous method"
    );
    let source:external::Instance=serde_json::from_value(json!({"pane":"herdr/fixture/w1:p1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":a.home,"thread_id":a.id,"rollout":rollout,"home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
    let s = crate::catalog::reconcile(&paths, &[source])
        .unwrap()
        .remove(0);
    let request = |cursor: Option<String>| Request {
        version: 1,
        session_id: s.id.clone(),
        expected_generation: s.generation(),
        expected_runtime_generation: a.generation(),
        expected_turn_id: "fixture-turn".into(),
        cursor,
    };
    let first = read(&paths, request(None)).await.unwrap();
    assert_eq!(first["history_mode"], "legacy");
    assert_eq!(first["events"][0]["text"], "Synthetic native checkpoint");
    let cursor = first["cursor"].as_str().unwrap().to_owned();
    let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    file.write_all(line("event_msg",json!({"type":"agent_message","phase":"commentary","message":"Synthetic native continuation"})).as_bytes()).unwrap();
    file.sync_all().unwrap();
    let second = read(&paths, request(Some(cursor))).await.unwrap();
    assert_eq!(second["events"][0]["text"], "Synthetic native continuation");
    assert_eq!(second["events"][0]["sequence"], 2);
    assert_eq!(second["sends_input"], false);
    assert!(!a.home.join("auth.json").exists());
}

#[test]
fn legacy_public_pagination_skips_private_items_and_preserves_exact_turn() {
    let make = || {
        let mut items = vec![
            serde_json::value::to_raw_value(
                &json!({"type":"reasoning","text":"PRIVATE_REASONING"}),
            )
            .unwrap(),
        ];
        for n in 0..101 {
            items.push(serde_json::value::to_raw_value(&json!({"type":"agentMessage","id":format!("item-{n}"),"text":format!("Public {n}"),"phase":"commentary"})).unwrap());
        }
        LegacyPage {
            data: vec![LegacyTurn {
                id: "tracked".into(),
                status: "inProgress".into(),
                view: "full".into(),
                items,
            }],
        }
    };
    let mut j = Journal::default();
    for (expected, more) in [(50, true), (100, true), (101, false)] {
        let (page, pending) = legacy_public(make(), "tracked", "inProgress", "scope", &j).unwrap();
        assert_eq!(pending, more);
        assert!(page.data.len() <= 50);
        j.ingest("scope", "tracked", page, 1).unwrap();
        assert_eq!(j.sequence, expected);
    }
    assert!(!serde_json::to_string(&j).unwrap().contains("PRIVATE"));
    assert!(legacy_public(make(), "different", "inProgress", "scope", &j).is_err());
    assert!(legacy_public(make(), "tracked", "completed", "scope", &j).is_err());
    let (empty, pending) = legacy_public(make(), "tracked", "inProgress", "scope", &j).unwrap();
    assert!(empty.data.is_empty() && !pending);
}

#[tokio::test]
async fn passive_turn_target_ignores_send_gates_but_rejects_turn_races() {
    use crate::messaging::tests::scripted_server;
    for host in ["%1", "herdr/fixture/w1:p1"] {
        for state in [
            "inProgress",
            "completed",
            "failed",
            "interrupted",
            "race",
            "empty",
        ] {
            let (_tmp, paths, input, _) = fixture();
            let mut a = attachment_fixture(&paths, &input);
            bind(&paths, &mut a);
            fs::create_dir_all(paths.root.join("conversations")).unwrap();
            fs::write(
                paths
                    .root
                    .join("conversations")
                    .join(format!("{}.json", a.id)),
                json!({"stop_intent":"keep-stop"}).to_string(),
            )
            .unwrap();
            let seen = Arc::new(Mutex::new(0));
            let calls = seen.clone();
            let id = a.id.clone();
            let server=scripted_server(&a.endpoint(),move |r|match r["method"].as_str().unwrap(){
                "thread/read"=>Some(json!({"thread":{"id":id,"historyMode":"legacy"}})),
                "thread/turns/list"=>{ let mut n=calls.lock().unwrap();*n+=1; let data=if state=="empty"{json!([])}else{json!([{"id":if state=="race"&&*n==2{"changed"}else{"tracked"},"status":if state=="race"{"inProgress"}else{state}}])};Some(json!({"data":data})) },
                method=>panic!("No mutating RPC: {method}"),
            }).await;
            let source:external::Instance=serde_json::from_value(json!({"pane":host,"tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
            let s = crate::catalog::reconcile(&paths, &[source])
                .unwrap()
                .remove(0);
            let result = target(&paths, &s.id, &s.generation()).await;
            if state == "race" {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("checkpoint_stale_turn")
                );
            } else {
                let v = result.unwrap();
                assert_eq!(v["available"], state != "empty");
                assert_eq!(v["sends_input"], false);
                if state != "empty" {
                    assert_eq!(v["turn_status"], state);
                    assert_eq!(v["turn_id"], "tracked");
                }
            }
            assert_eq!(
                external::messaging_stop_intent(&paths, &a.id)
                    .unwrap()
                    .as_deref(),
                Some("keep-stop")
            );
            server.abort();
        }
    }
}
