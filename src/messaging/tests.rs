use super::*;
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::Message;

pub(super) fn fixture() -> (tempfile::TempDir, Paths, Enqueue, i64) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    fs::create_dir_all(&paths.root).unwrap();
    let data: Value =
        serde_json::from_str(include_str!("../../docs/messaging/v1.2.1/fixtures.json")).unwrap();
    (
        dir,
        paths,
        serde_json::from_value(data["enqueue"].clone()).unwrap(),
        data["now"].as_i64().unwrap(),
    )
}
#[test]
fn contract_fixture_receipts_and_restart_dedup() {
    let (_dir, paths, input, now) = fixture();
    let first = {
        let mut db = Ledger::open(&paths).unwrap();
        db.accept(&input, now).unwrap()
    };
    let expected: Value =
        serde_json::from_str(include_str!("../../docs/messaging/v1.2.1/fixtures.json")).unwrap();
    assert_eq!(json!(first), expected["local_receipt"]);
    let mut db = Ledger::open(&paths).unwrap();
    assert_eq!(db.accept(&input, now + 999).unwrap(), first); // retry after original deadline still deduplicates
    let mut changed = input.clone();
    changed.text = "different".into();
    assert_eq!(
        db.accept(&changed, now).unwrap_err().0,
        "message_id_conflict"
    );
    assert_eq!(
        db.events(None).unwrap()["events"].as_array().unwrap().len(),
        1
    );
    db.db
        .execute(
            "UPDATE metadata SET epoch=?",
            [expected["cursor_epoch"].as_str().unwrap()],
        )
        .unwrap();
    assert_eq!(
        json!({"contract_version":VERSION,"ok":true,"result":db.events(None).unwrap()}),
        expected["event_page"]
    );
}
#[test]
fn expiry_only_affects_provably_unsent() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    db.expire(&mut receipt, input.expires_at).unwrap();
    assert_eq!(receipt.state, "expired");
    receipt.state = "unknown".into();
    db.save(&receipt).unwrap();
    db.expire(&mut receipt, input.expires_at + 1).unwrap();
    assert_eq!(receipt.state, "unknown");
    receipt.state = "native_accepted".into();
    db.save(&receipt).unwrap();
    db.expire(&mut receipt, input.expires_at + 1).unwrap();
    assert_eq!(receipt.state, "native_accepted");
}
#[test]
fn ids_sizes_expiry_and_conflicting_generation() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    assert!(uuid("my-session").is_err());
    assert!(uuid("pane:%1").is_err());
    let mut bad = input.clone();
    bad.text = "é".repeat(8193);
    assert_eq!(db.accept(&bad, now).unwrap_err().0, "invalid_request");
    bad = input.clone();
    bad.expires_at = now;
    assert_eq!(db.accept(&bad, now).unwrap_err().0, "invalid_expiry");
    db.accept(&input, now).unwrap();
    bad = input.clone();
    bad.expected_generation = "b".repeat(64);
    assert_eq!(db.accept(&bad, now).unwrap_err().0, "message_id_conflict");
}
#[test]
fn event_order_replay_retention_and_epoch_gaps() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    let receipt = db.accept(&input, now).unwrap();
    let start = db.events(None).unwrap();
    assert_eq!(start, db.events(None).unwrap());
    let old = start["cursor"].as_str().unwrap();
    for _ in 0..1001 {
        db.save(&receipt).unwrap();
    }
    assert_eq!(db.events(Some(old)).unwrap_err().0, "cursor_gap");
    assert_eq!(db.events(Some("other:1")).unwrap_err().0, "cursor_gap");
    let page = db.events(None).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 100);
    assert_eq!(page["has_more"], true);
    let next = db.events(page["cursor"].as_str()).unwrap();
    assert_ne!(page["events"][0]["cursor"], next["events"][0]["cursor"]);
    assert_eq!(
        db.db
            .query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1000
    );
}
#[test]
fn bounded_ledger_never_evicts_deduplication_keys() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    db.db.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO receipts SELECT x,'hash',0,'{}' FROM n;").unwrap();
    assert_eq!(db.accept(&input, now).unwrap_err().0, "receipt_capacity");
}
#[tokio::test]
async fn cancel_unsent_and_unknown_without_native_control() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    cancel(&paths, &mut db, &mut receipt).await.unwrap();
    assert_eq!(receipt.state, "cancelled");
    receipt.state = "unknown".into();
    db.save(&receipt).unwrap();
    assert_eq!(
        cancel(&paths, &mut db, &mut receipt).await.unwrap_err().0,
        "unknown"
    );
    receipt.state = "execution_started".into();
    assert_eq!(
        cancel(&paths, &mut db, &mut receipt).await.unwrap_err().0,
        "already_started"
    );
}
pub(super) fn attachment_fixture(paths: &Paths, input: &Enqueue) -> Attachment {
    let home = paths.root.join("fake-home");
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    Attachment {
        id: input.session_id.clone(),
        home,
        pid: 123,
        start: 42,
    }
}
#[test]
fn logical_identity_generation_and_ambiguity() {
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let original = a.generation();
    assert_eq!(
        select(vec![a.clone()], &a.id).unwrap().generation(),
        original
    );
    let mut b = a.clone();
    b.home = paths.root.join("another-account");
    assert_ne!(b.generation(), original);
    b = a.clone();
    b.start += 1;
    assert_ne!(b.generation(), original);
    assert!(matches!(
        select(vec![a.clone(), b], &a.id),
        Err(Fault("ambiguous_session"))
    ));
    assert!(matches!(
        select(vec![a], "33333333-3333-4333-8333-333333333333"),
        Err(Fault("unavailable"))
    ));
}
#[test]
fn movement_and_message_share_locks() {
    let (_dir, paths, input, _) = fixture();
    let first = mutation_lock(&paths, &input.session_id).unwrap();
    assert_eq!(
        mutation_lock(&paths, &input.session_id).unwrap_err().0,
        "operation_busy"
    );
    let lock = lock_file(&paths.root.join("handoff-jobs/queue.lock")).unwrap();
    assert!(lock.try_lock_exclusive().is_err());
    drop(first);
    assert!(mutation_lock(&paths, &input.session_id).is_ok());
    // Incomplete persisted jobs fail closed instead of assuming a vanished worker is safe.
    fs::write(paths.root.join("handoff-jobs/broken.json"), b"{").unwrap();
    assert!(mutation_lock(&paths, &input.session_id).is_err());
}

#[test]
fn handoff_reserves_delivery_until_worker_releases_even_after_terminal_phase() {
    let (_dir, paths, input, _) = fixture();
    let root = paths.root.join("handoff-jobs");
    crate::paths::private_dir(&root).unwrap();
    let worker = lock_file(&root.join("switch.worker-lock")).unwrap();
    let source = json!({"pane":"%1","tmux_session":"fixture","pid":1,
        "cwd":"/fixture","thread_id":input.session_id,"rollout":"/fixture/history",
        "home":"/fixture/home","binary":"/fixture/codex","resume_options":[]});
    for phase in [
        "queued",
        "waiting_for_idle",
        "complete",
        "failed",
        "cancelled",
        "recovered",
    ] {
        fs::write(
            root.join("switch.json"),
            serde_json::to_vec(&json!({
                "id":"switch", "source":source,"target":"other","phase":phase,"detail":""
            }))
            .unwrap(),
        )
        .unwrap();
        worker.lock_exclusive().unwrap();
        assert_eq!(
            mutation_lock(&paths, &input.session_id).unwrap_err().0,
            "migrating",
            "{phase}"
        );
        // Another conversation is independent of this reservation.
        assert!(mutation_lock(&paths, "33333333-3333-4333-8333-333333333333").is_ok());
        FileExt::unlock(&worker).unwrap();
        if matches!(phase, "queued" | "waiting_for_idle") {
            assert_eq!(
                mutation_lock(&paths, &input.session_id).unwrap_err().0,
                "migrating"
            );
        } else {
            assert!(mutation_lock(&paths, &input.session_id).is_ok(), "{phase}");
        }
    }
}

struct Server {
    task: tokio::task::JoinHandle<()>,
    methods: Arc<Mutex<Vec<String>>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(
    path: &std::path::Path,
    version: &str,
    busy: bool,
    lose_add: bool,
    delete: bool,
) -> Server {
    let listener = UnixListener::bind(path).unwrap();
    let methods = Arc::new(Mutex::new(Vec::new()));
    let seen = methods.clone();
    let version = version.to_string();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let method = req["method"].as_str().unwrap();
                seen.lock().unwrap().push(method.to_string());
                if method == "initialized" {
                    continue;
                }
                // Private/unrelated notifications and duplicate/out-of-order IDs cannot steal the response.
                for frame in [
                    json!({"id":req["id"],"method":"item/commandExecution/requestApproval","params":{"private":"SECRET_TOOL"}}),
                    json!({"method":"item/reasoning/textDelta","params":{"delta":"SECRET_REASONING"}}),
                    json!({"id":999999,"result":{"private":"SECRET_TOOL"}}),
                    json!({"method":"thread/queue/changed","params":{"threadId":req["params"]["threadId"]}}),
                ] {
                    if socket
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                let result = match method {
                    "initialize" => {
                        json!({"userAgent":format!("codex_cli_rs/{version} (Linux)"),"codexHome":"SECRET_HOME"})
                    }
                    "thread/read" => {
                        json!({"thread":{"id":req["params"]["threadId"],"status":{"type":if busy{"active"}else{"idle"}},"turns":[{"reasoning":"SECRET_REASONING"}]}})
                    }
                    "thread/turns/list" => {
                        json!({"data":[{"id":"last-turn","status":"completed"}]})
                    }
                    "thread/queue/add" => {
                        if lose_add {
                            break;
                        }
                        json!({"queuedSubmission":{"id":"queue-1","clientUserMessageId":req["params"]["clientUserMessageId"],"input":[{"private":"SECRET_TOOL"}]}})
                    }
                    "thread/queue/list" => {
                        json!({"data":[{"id":"queue-1","clientUserMessageId":"11111111-1111-4111-8111-111111111111","input":[{"private":"SECRET_TOOL"}]}],"nextCursor":null})
                    }
                    "thread/queue/delete" => json!({"deleted":delete}),
                    _ => panic!("forbidden method: {method}"),
                };
                if socket
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
    Server { task, methods }
}
#[tokio::test]
async fn supported_queue_busy_idle_interleaving_and_no_private_leakage() {
    for busy in [false, true] {
        let (_dir, paths, input, now) = fixture();
        let a = attachment_fixture(&paths, &input);
        let fake = server(&a.endpoint(), "0.163.0", busy, false, true).await;
        let mut control = a.connect().await.unwrap();
        assert_eq!(
            activity(&mut control, &input.session_id).await.unwrap(),
            if busy { "busy" } else { "idle" }
        );
        let mut db = Ledger::open(&paths).unwrap();
        let mut receipt = db.accept(&input, now).unwrap();
        assert_eq!(receipt.state, "pending_local");
        submit(&mut control, &mut db, &mut receipt, &input)
            .await
            .unwrap();
        assert_eq!(receipt.state, "native_accepted");
        let events = db.events(None).unwrap().to_string();
        assert!(!events.contains("SECRET"));
        assert!(!events.contains(&input.text));
        let bytes = fs::read(paths.root.join("messaging/receipts.db")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("SECRET"));
        assert!(!String::from_utf8_lossy(&bytes).contains(&input.text));
        for forbidden in [
            "turn/steer",
            "turn/start",
            "thread/resume",
            "thread/queue/start",
        ] {
            assert!(
                control
                    .request::<Value>(forbidden, json!({}))
                    .await
                    .is_err()
            );
        }
        assert!(
            !fake
                .methods
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "turn/start")
        );
    }
}
#[tokio::test]
async fn unsupported_versions_and_missing_endpoint_do_not_launch() {
    for version in ["0.162.1", "0.164.0", "0.163.01"] {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let fake = server(&a.endpoint(), version, false, false, true).await;
        assert!(matches!(a.connect().await, Err(Fault("unsupported"))));
        assert_eq!(fake.methods.lock().unwrap().as_slice(), ["initialize"]);
    }
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    assert!(matches!(
        a.connect().await,
        Err(Fault("endpoint_unavailable"))
    ));
}
#[tokio::test]
async fn lost_acceptance_survives_restart_and_reconciles_without_resubmitting() {
    let (_dir, paths, input, now) = fixture();
    let a = attachment_fixture(&paths, &input);
    let fake = server(&a.endpoint(), "0.163.0", true, true, true).await;
    {
        let mut control = a.connect().await.unwrap();
        let mut db = Ledger::open(&paths).unwrap();
        let mut receipt = db.accept(&input, now).unwrap();
        submit(&mut control, &mut db, &mut receipt, &input)
            .await
            .unwrap();
        assert_eq!(receipt.state, "unknown");
    }
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    assert_eq!(receipt.state, "unknown");
    let mut control = a.connect().await.unwrap();
    reconcile(&mut control, &mut db, &mut receipt)
        .await
        .unwrap();
    assert_eq!(receipt.state, "native_accepted");
    assert_eq!(
        fake.methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "thread/queue/add")
            .count(),
        1
    );
}
#[tokio::test]
async fn stale_generation_blocks_actual_submission() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    a.pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{}/stat", a.pid)).unwrap();
    a.start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    fs::create_dir_all(paths.root.join("managed-panes")).unwrap();
    fs::write(
        paths.root.join("managed-panes/1.json"),
        json!({"pid":a.pid,"start_time":a.start,"home":a.home,"thread":a.id}).to_string(),
    )
    .unwrap();
    let fake = server(&a.endpoint(), "0.163.0", true, false, true).await;
    input.expected_generation = "0".repeat(64);
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    assert_eq!(
        deliver(&paths, &mut db, &mut receipt, Some(&input))
            .await
            .unwrap_err()
            .0,
        "generation_mismatch"
    );
    assert!(fake.methods.lock().unwrap().is_empty());
}

pub(super) fn bind(paths: &Paths, a: &mut Attachment) {
    a.pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{}/stat", a.pid)).unwrap();
    a.start = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    fs::create_dir_all(paths.root.join("managed-panes")).unwrap();
    fs::write(
        paths.root.join("managed-panes/1.json"),
        json!({"pid":a.pid,"start_time":a.start,"home":a.home,"thread":a.id}).to_string(),
    )
    .unwrap();
}
#[tokio::test]
async fn native_cancellation_true_vs_too_late() {
    for deleted in [true, false] {
        let (_dir, paths, mut input, now) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let fake = server(&a.endpoint(), "0.163.0", true, false, deleted).await;
        input.expected_generation = a.generation();
        let mut db = Ledger::open(&paths).unwrap();
        let mut receipt = db.accept(&input, now).unwrap();
        deliver(&paths, &mut db, &mut receipt, Some(&input))
            .await
            .unwrap();
        assert_eq!(receipt.state, "native_accepted");
        let outcome = cancel(&paths, &mut db, &mut receipt).await;
        if deleted {
            outcome.unwrap();
            assert_eq!(receipt.state, "cancelled");
        } else {
            assert_eq!(outcome.unwrap_err().0, "already_submitted");
            assert_ne!(receipt.state, "cancelled");
        }
        assert!(
            fake.methods
                .lock()
                .unwrap()
                .iter()
                .all(|s| !s.contains("interrupt") && !s.contains("steer"))
        );
    }
}

pub(super) async fn scripted_server(
    path: &std::path::Path,
    handler: impl Fn(&Value) -> Option<Value> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
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
                .send(Message::Text(
                    json!({"id":req["id"],"result":result}).to_string().into(),
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}
#[tokio::test]
async fn revalidates_generation_after_native_preflight() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let binding = paths.root.join("managed-panes/1.json");
    let task = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/read");
        fs::remove_file(&binding).unwrap();
        Some(json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"idle"}}}))
    })
    .await;
    input.expected_generation = a.generation();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    assert!(
        deliver(&paths, &mut db, &mut receipt, Some(&input))
            .await
            .is_err()
    );
    assert_eq!(receipt.state, "pending_local");
    task.abort();
}
#[tokio::test]
async fn missing_native_evidence_never_replays_and_duplicate_correlation_is_rejected() {
    for duplicate in [false, true] {
        let (_dir, paths, input, now) = fixture();
        let a = attachment_fixture(&paths, &input);
        let id = input.message_id.clone();
        let task=scripted_server(&a.endpoint(),move|req|{
            assert_eq!(req["method"],"thread/queue/list");
            Some(if duplicate {json!({"data":[{"id":"q1","clientUserMessageId":id},{"id":"q2","clientUserMessageId":id}],"nextCursor":null})}else{json!({"data":[],"nextCursor":null})})
        }).await;
        let mut db = Ledger::open(&paths).unwrap();
        let mut receipt = db.accept(&input, now).unwrap();
        receipt.state = "unknown".into();
        db.save(&receipt).unwrap();
        let mut control = a.connect().await.unwrap();
        let outcome = reconcile(&mut control, &mut db, &mut receipt).await;
        if duplicate {
            assert_eq!(outcome.unwrap_err().0, "ambiguous_native_correlation");
        } else {
            outcome.unwrap();
        }
        assert_eq!(receipt.state, "unknown");
        task.abort();
    }
}
#[tokio::test]
async fn lost_native_cancellation_response_is_unknown() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let task = scripted_server(&a.endpoint(), |req| {
        assert_eq!(req["method"], "thread/queue/delete");
        None
    })
    .await;
    input.expected_generation = a.generation();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    receipt.state = "native_accepted".into();
    receipt.native_queue_id = Some("queue-1".into());
    db.save(&receipt).unwrap();
    assert_eq!(
        cancel(&paths, &mut db, &mut receipt).await.unwrap_err().0,
        "unknown"
    );
    assert_eq!(db.get(&input.message_id).unwrap().unwrap().state, "unknown");
    task.abort();
}
#[tokio::test]
async fn malformed_and_oversized_frames_fail_closed() {
    for frame in [
        "not json".to_string(),
        format!(
            "{{\"method\":\"private\",\"params\":\"{}\"}}",
            "x".repeat(1024 * 1024)
        ),
    ] {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let listener = UnixListener::bind(a.endpoint()).unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = socket.next().await;
            let _ = socket.send(Message::Text(frame.into())).await;
        });
        assert!(a.connect().await.is_err());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn submission_holds_movement_locks_and_journals_before_native_mutation() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let root = paths.root.clone();
    let task=scripted_server(&a.endpoint(),move|req|{
        let queue=lock_file(&root.join("handoff-jobs/queue.lock")).unwrap();assert!(queue.try_lock_exclusive().is_err());
        let conversation=lock_file(&root.join(format!("handoff-jobs/{}.open-lock",req["params"]["threadId"].as_str().unwrap()))).unwrap();assert!(conversation.try_lock_exclusive().is_err());
        if req["method"]=="thread/read" {return Some(json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"active"}}}));}
        assert_eq!(req["method"],"thread/queue/add");
        let db=Connection::open(root.join("messaging/receipts.db")).unwrap();let text:String=db.query_row("SELECT receipt FROM receipts",[],|r|r.get(0)).unwrap();let persisted:Receipt=serde_json::from_str(&text).unwrap();assert_eq!(persisted.state,"unknown");
        Some(json!({"queuedSubmission":{"id":"q","clientUserMessageId":req["params"]["clientUserMessageId"]}}))
    }).await;
    input.expected_generation = a.generation();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    deliver(&paths, &mut db, &mut receipt, Some(&input))
        .await
        .unwrap();
    assert_eq!(receipt.state, "native_accepted");
    task.abort();
}
#[tokio::test]
async fn already_queued_migration_rejects_enqueue_before_any_socket_use() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let fake = server(&a.endpoint(), "0.163.0", true, false, true).await;
    let dir = paths.root.join("handoff-jobs");
    fs::create_dir_all(&dir).unwrap();
    let job = json!({"id":"job","source":{"pane":"%1","tmux_session":"fake","pid":a.pid,"start_time":a.start,"cwd":"/fixture","thread_id":a.id,"rollout":"/fixture/unused","home":a.home,"binary":"unused","resume_options":[]},"target":"other","phase":"queued","detail":"fixture","source_lease_released":false,"pause_approved":false,"risks_approved":false});
    let _: external::Job = serde_json::from_value(job.clone()).unwrap();
    fs::write(dir.join("job.json"), job.to_string()).unwrap();
    input.expected_generation = a.generation();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    assert_eq!(
        deliver(&paths, &mut db, &mut receipt, Some(&input))
            .await
            .unwrap_err()
            .0,
        "migrating"
    );
    assert_eq!(receipt.state, "pending_local");
    assert!(fake.methods.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_submission_timeout_retains_uncertainty() {
    let (_dir, paths, input, now) = fixture();
    let a = attachment_fixture(&paths, &input);
    let listener = UnixListener::bind(a.endpoint()).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let req: Value = serde_json::from_str(&text).unwrap();
            match req["method"].as_str().unwrap() {
                "initialize" => {
                    socket
                        .send(Message::Text(
                            json!({"id":req["id"],"result":{"userAgent":"codex_cli_rs/0.163.0"}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
                "initialized" => {}
                "thread/queue/add" => {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    break;
                }
                _ => panic!("unexpected request"),
            }
        }
    });
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    let mut control = a.connect().await.unwrap();
    submit(&mut control, &mut db, &mut receipt, &input)
        .await
        .unwrap();
    assert_eq!(receipt.state, "unknown");
    assert_eq!(receipt.reason.as_deref(), Some("submission_uncertain"));
    task.abort();
}

#[tokio::test]
async fn native_socket_symlink_is_validated_and_target_replacement_changes_generation() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let private = paths.root.join("native-daemon");
    fs::create_dir(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    let target = private.join("socket");
    symlink(&target, a.endpoint()).unwrap();
    assert_eq!(a.connect().await.err().unwrap().0, "endpoint_unavailable");
    fs::write(&target, b"not a socket").unwrap();
    assert_eq!(a.connect().await.err().unwrap().0, "unsafe_endpoint");
    fs::remove_file(&target).unwrap();
    let task = scripted_server(&target, |req| {
        assert_eq!(req["method"], "thread/read");
        Some(json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"active"}}}))
    })
    .await;
    let generation = a.generation();
    let mut control = a.connect().await.unwrap();
    assert_eq!(activity(&mut control, &a.id).await.unwrap(), "busy");
    drop(control);
    task.abort();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(a.connect().await.err().unwrap().0, "unsafe_endpoint");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&target, private.join("old-socket")).unwrap();
    let _new = UnixListener::bind(&target).unwrap();
    assert_ne!(generation, a.generation());
    fs::set_permissions(
        a.endpoint().parent().unwrap(),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert_eq!(a.connect().await.err().unwrap().0, "unsafe_endpoint");
}

#[tokio::test]
async fn session_labels_are_bounded_metadata_not_identity_or_credentials() {
    let (_dir, paths, input, _) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    let gen_before = a.generation();
    assert_eq!(labels(&paths, &a.id, Some(&a.home)).unwrap(), (None, None));
    let shared: Value =
        serde_json::from_str(include_str!("../../docs/messaging/v1.2.1/fixtures.json")).unwrap();
    let other = shared["duplicate_name_session"]["session_id"]
        .as_str()
        .unwrap();
    fs::create_dir_all(paths.root.join("conversations")).unwrap();
    let write_name = |id: &str, name: &str| {
        fs::write(
            paths.root.join("conversations").join(format!("{id}.json")),
            json!({"name":name}).to_string(),
        )
        .unwrap();
    };
    let name = shared["named_session"]["display_name"].as_str().unwrap();
    write_name(&a.id, name);
    write_name(other, name);
    let first = resolve(&paths, &a.id).await.unwrap();
    let second = resolve(&paths, other).await.unwrap();
    assert_eq!(first["display_name"], name);
    assert_eq!(first["display_name"], second["display_name"]);
    assert_ne!(first["session_id"], second["session_id"]);
    assert!(resolve(&paths, name).await.is_err());
    write_name(&a.id, "renamed");
    assert_eq!(
        labels(&paths, &a.id, None).unwrap().0.as_deref(),
        Some("renamed")
    );
    assert_eq!(a.generation(), gen_before);
    write_name(&a.id, &format!("\u{1b}{}", "界".repeat(100)));
    let bounded = labels(&paths, &a.id, None).unwrap().0.unwrap();
    assert_eq!(bounded, "界".repeat(42));
    write_name(&a.id, "\n\t");
    assert_eq!(labels(&paths, &a.id, None).unwrap().0, None);
    for account in ["personal", "work"] {
        a.home = paths.root.join("accounts").join(account);
        fs::create_dir_all(&a.home).unwrap();
        // Invalid credential fixture proves this mapping does not parse auth.
        fs::write(a.home.join("auth.json"), "must not be parsed").unwrap();
        assert_eq!(
            labels(&paths, &a.id, Some(&a.home)).unwrap().1.as_deref(),
            Some(account)
        );
    }
}

#[tokio::test]
async fn public_requests_deduplicate_without_retargeting_or_steering() {
    for (mode, busy, lost) in [
        (PublicMode::Submit, true, false),
        (PublicMode::Queue, true, false),
        (PublicMode::Submit, false, true),
    ] {
        let (_dir, paths, mut input, _) = fixture();
        input.expires_at = crate::daemon::now() + 120;
        let mut a = attachment_fixture(&paths, &input);
        a.pid = std::process::id();
        a.start = fs::read_to_string(format!("/proc/{}/stat", a.pid))
            .unwrap()
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        fs::create_dir_all(paths.root.join("managed-panes")).unwrap();
        fs::write(
            paths.root.join("managed-panes/1.json"),
            json!({"pid":a.pid,"start_time":a.start,"home":a.home,"thread":a.id}).to_string(),
        )
        .unwrap();
        let fake = server(&a.endpoint(), "0.163.0", busy, lost, true).await;
        let instance:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"test","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let session = crate::catalog::reconcile(&paths, &[instance])
            .unwrap()
            .remove(0);
        let request = PublicRequest {
            recovery: None,
            api_version: 1,
            message_id: input.message_id.clone(),
            session_id: session.id.clone(),
            expected_generation: session.generation(),
            mode: mode.clone(),
            text: "hello".into(),
            expires_at: input.expires_at,
        };
        let first = send_public(&paths, request.clone()).await.unwrap();
        assert_eq!(first["session_id"], session.id);
        if mode == PublicMode::Submit && busy {
            assert_eq!(first["state"], "rejected");
            assert_eq!(first["reason"], "target_busy");
        } else if lost {
            assert_eq!(first["state"], "delivery_unknown");
        } else {
            assert_eq!(first["state"], "native_accepted");
        }
        // A new backend generation cannot turn a retry into new delivery.
        crate::catalog::rename(&paths, &session.id, &session.generation(), "changed").unwrap();
        send_public(&paths, request.clone()).await.unwrap();
        let mut changed = request;
        changed.text = "changed".into();
        assert!(
            send_public(&paths, changed)
                .await
                .unwrap_err()
                .to_string()
                .contains("MESSAGE_ID_CONFLICT")
        );
        let calls = fake.methods.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|m| *m == "thread/queue/add").count(),
            usize::from(!(mode == PublicMode::Submit && busy))
        );
        assert!(!calls.iter().any(|m| m == "turn/steer" || m == "turn/start"));
    }
}

#[tokio::test]
async fn ordinary_public_send_requires_explicit_recovery_for_terminal_turns() {
    for mode in [PublicMode::Submit, PublicMode::Queue] {
        for (state, last) in [
            ("idle", "interrupted"),
            ("idle", "failed"),
            ("systemError", "failed"),
        ] {
            let (_dir, paths, input, _) = fixture();
            let mut a = attachment_fixture(&paths, &input);
            bind(&paths, &mut a);
            let id = a.id.clone();
            let server = scripted_server(&a.endpoint(), move |r| {
                match r["method"].as_str().unwrap() {
                    "thread/read" => Some(json!({"thread":{"id":id,"status":{"type":state}}})),
                    "thread/turns/list" => Some(json!({"data":[{"id":"previous","status":last}]})),
                    method => panic!("Terminal state must not receive native input: {method}"),
                }
            })
            .await;
            let instance:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
            let session = crate::catalog::reconcile(&paths, &[instance])
                .unwrap()
                .remove(0);
            let request = PublicRequest {
                api_version: 1,
                message_id: input.message_id,
                session_id: session.id.clone(),
                expected_generation: session.generation(),
                mode: mode.clone(),
                recovery: None,
                text: "must not be queued".into(),
                expires_at: crate::daemon::now() + 120,
            };
            let result = send_public(&paths, request.clone()).await.unwrap();
            assert_eq!(result["state"], "rejected");
            assert_eq!(result["reason"], "explicit_resume_required");
            assert_eq!(result["facts"]["accepted_native"], false);
            assert_eq!(
                send_public(&paths, request).await.unwrap()["state"],
                "rejected"
            );
            server.abort();
        }
    }
}
