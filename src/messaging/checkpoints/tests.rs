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
    for host in ["%1", "herdr/fixture/w1:p1"] {
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
                        "thread/turns/list" => {
                            json!({"data":[{"id":if mode=="stale"{"other"}else{"tracked"},"status":if mode=="failed"{"failed"}else{"inProgress"}}]})
                        }
                        "thread/items/list" => {
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
            ("wrong", "checkpoint_turn_mismatch"),
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
            "initialize" | "thread/turns/list" | "thread/items/list"
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
