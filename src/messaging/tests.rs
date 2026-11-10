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
    let lock = lock_file(
        &paths
            .root
            .join(crate::layout::HANDOFF_JOBS)
            .join("queue.lock"),
    )
    .unwrap();
    assert!(lock.try_lock_exclusive().is_err());
    drop(first);
    assert!(mutation_lock(&paths, &input.session_id).is_ok());
    // Incomplete persisted jobs fail closed instead of assuming a vanished worker is safe.
    fs::write(
        paths
            .root
            .join(crate::layout::HANDOFF_JOBS)
            .join("broken.json"),
        b"{",
    )
    .unwrap();
    assert!(mutation_lock(&paths, &input.session_id).is_err());
}

#[test]
fn handoff_reserves_delivery_until_worker_releases_even_after_terminal_phase() {
    let (_dir, paths, input, _) = fixture();
    let root = paths.root.join(crate::layout::HANDOFF_JOBS);
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
    version: Arc<Mutex<String>>,
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
    let version = Arc::new(Mutex::new(version.to_string()));
    let profile = version.clone();
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
                        json!({"userAgent":format!("codex_cli_rs/{} (Linux)", profile.lock().unwrap()),"codexHome":"SECRET_HOME"})
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
    Server {
        task,
        methods,
        version,
    }
}
#[tokio::test]
async fn supported_queue_busy_idle_interleaving_and_no_private_leakage() {
    for (version, busy) in NATIVE_PROFILES.iter().flat_map(|v| [(v, false), (v, true)]) {
        let (_dir, paths, input, now) = fixture();
        let a = attachment_fixture(&paths, &input);
        let fake = server(&a.endpoint(), version, busy, false, true).await;
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
    // Unknown future releases (minor and patch bumps) fail closed at the
    // handshake, before any thread method, until a profile is verified.
    for version in [
        "0.162.1",
        "0.167.0",
        "0.166.1",
        "1.0.0",
        "0.163.01",
        "0.164.0-preview",
        "0.166.0-alpha.1",
    ] {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let fake = server(&a.endpoint(), version, false, false, true).await;
        let mut profile = None;
        assert!(matches!(
            a.connect_with_profile(&mut profile).await,
            Err(Fault("unsupported_native_version"))
        ));
        assert_eq!(profile.as_deref(), Some(version));
        assert_eq!(fake.methods.lock().unwrap().as_slice(), ["initialize"]);
    }
    // Every exact profile, including 0.165.0 and 0.166.0 (generated-schema diffs
    // in docs/operations.md), passes the same handshake gate.
    assert!(NATIVE_PROFILES.contains(&"0.166.0"));
    for version in NATIVE_PROFILES {
        let (_dir, paths, input, _) = fixture();
        let a = attachment_fixture(&paths, &input);
        let _fake = server(&a.endpoint(), version, false, false, true).await;
        let mut profile = None;
        assert!(
            a.connect_with_profile(&mut profile).await.is_ok(),
            "{version}"
        );
        assert_eq!(profile.as_deref(), Some(*version));
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
    fs::create_dir_all(paths.root.join(crate::layout::MANAGED_PANES)).unwrap();
    fs::write(
        paths.root.join(crate::layout::MANAGED_PANES).join("1.json"),
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
    fs::create_dir_all(paths.root.join(crate::layout::MANAGED_PANES)).unwrap();
    fs::write(
        paths.root.join(crate::layout::MANAGED_PANES).join("1.json"),
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
    scripted_connections(path, handler, 1).await
}

pub(super) async fn scripted_connections(
    path: &std::path::Path,
    handler: impl Fn(&Value) -> Option<Value> + Send + 'static,
    connections: usize,
) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        for _ in 0..connections {
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
        }
    })
}
#[tokio::test]
async fn revalidates_generation_after_native_preflight() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let binding = paths.root.join(crate::layout::MANAGED_PANES).join("1.json");
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
        let queue=lock_file(&root.join(crate::layout::HANDOFF_JOBS).join("queue.lock")).unwrap();assert!(queue.try_lock_exclusive().is_err());
        let conversation=lock_file(&root.join(crate::layout::HANDOFF_JOBS).join(format!("{}.open-lock",req["params"]["threadId"].as_str().unwrap()))).unwrap();assert!(conversation.try_lock_exclusive().is_err());
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
    let dir = paths.root.join(crate::layout::HANDOFF_JOBS);
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
    fs::create_dir_all(paths.root.join(crate::layout::CONVERSATIONS)).unwrap();
    let write_name = |id: &str, name: &str| {
        fs::write(
            paths
                .root
                .join(crate::layout::CONVERSATIONS)
                .join(format!("{id}.json")),
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
        a.home = paths.account(account).unwrap();
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
    for (mode, busy, lost, if_busy) in [
        (PublicMode::Submit, true, false, None),
        // Opt-in: a submit that lost the race with a starting turn is queued.
        (PublicMode::Submit, true, false, Some("queue")),
        (PublicMode::Queue, true, false, None),
        (PublicMode::Submit, false, true, None),
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
        fs::create_dir_all(paths.root.join(crate::layout::MANAGED_PANES)).unwrap();
        fs::write(
            paths.root.join(crate::layout::MANAGED_PANES).join("1.json"),
            json!({"pid":a.pid,"start_time":a.start,"home":a.home,"thread":a.id}).to_string(),
        )
        .unwrap();
        let fake = server(
            &a.endpoint(),
            if mode == PublicMode::Queue {
                "0.167.0"
            } else {
                "0.163.0"
            },
            busy,
            lost,
            true,
        )
        .await;
        let instance:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"test","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let session = crate::catalog::reconcile(&paths, &[instance])
            .unwrap()
            .remove(0);
        let request = PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: if_busy.map(String::from),
            steering: None,
            recovery: None,
            api_version: 1,
            message_id: input.message_id.clone(),
            session_id: session.id.clone(),
            expected_generation: session.generation(),
            mode: mode.clone(),
            text: "hello".into(),
            expires_at: input.expires_at,
        };
        if mode == PublicMode::Queue {
            let blocked = send_public(&paths, request.clone()).await.unwrap();
            assert_eq!(blocked["state"], "pending_local");
            assert_eq!(blocked["reason"], "unsupported_native_version");
            assert_eq!(blocked["facts"]["accepted_native"], false);
            assert!(
                blocked["reason_detail"]
                    .as_str()
                    .unwrap()
                    .contains("app-server")
            );
            let status = observe_public(&paths, &session.id, &session.generation())
                .await
                .unwrap();
            assert_eq!(
                status["observation"]["native_protocol"]["version"],
                "0.167.0"
            );
            assert_eq!(status["observation"]["native_protocol"]["supported"], false);
            assert_eq!(
                status["observation"]["failure"]["code"],
                "unsupported_native_version"
            );
            assert_eq!(
                fake.methods
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| *m == "thread/queue/add")
                    .count(),
                0
            );
            // Same endpoint and request after a compatible backend becomes available.
            // No duplicate ID, payload change, or implicit replay on observation.
            *fake.version.lock().unwrap() = "0.166.0".into();
        }
        let first = send_public(&paths, request.clone()).await.unwrap();
        assert_eq!(first["session_id"], session.id);
        if mode == PublicMode::Submit && busy && if_busy.is_none() {
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
            usize::from(!(mode == PublicMode::Submit && busy && if_busy.is_none()))
        );
        assert!(!calls.iter().any(|m| m == "turn/steer" || m == "turn/start"));
    }
}

#[tokio::test]
async fn ordinary_public_send_requires_explicit_recovery_for_interrupted_turns() {
    for mode in [PublicMode::Submit, PublicMode::Queue] {
        for (state, last) in [("idle", "interrupted")] {
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
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                steering: None,
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

#[tokio::test]
async fn approval_blocked_public_send_is_a_definite_refusal_never_dispatched_on_resend() {
    for mode in [PublicMode::Submit, PublicMode::Queue] {
        let (_dir, paths, input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let id = a.id.clone();
        let server = scripted_server(&a.endpoint(), move |r| match r["method"]
            .as_str()
            .unwrap()
        {
            "thread/read" => Some(
                json!({"thread":{"id":id,"status":{"type":"active","activeFlags":["waitingOnApproval"]}}}),
            ),
            "thread/turns/list" => Some(json!({"data":[{"id":"t","status":"inProgress"}]})),
            method => panic!("An approval-blocked session must not receive input: {method}"),
        })
        .await;
        let instance:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let session = crate::catalog::reconcile(&paths, &[instance])
            .unwrap()
            .remove(0);
        let request = PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: None,
            steering: None,
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
        assert_eq!(result["state"], "rejected", "{result}");
        assert_eq!(result["reason"], "approval_or_user_input_required");
        assert_eq!(result["facts"]["accepted_native"], false);
        // A resend is a lookup of the definite refusal, never a later dispatch.
        assert_eq!(
            send_public(&paths, request).await.unwrap()["state"],
            "rejected"
        );
        server.abort();
    }
}

#[tokio::test]
async fn open_wait_outlasts_a_brief_holder_then_reports_busy() {
    let (_dir, paths, _, _) = fixture();
    let holder = Ledger::open(&paths).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(holder);
    });
    let started = std::time::Instant::now();
    let waited = Ledger::open_wait(&paths, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    // Budget exhausted: only then adapter_busy.
    let busy = Ledger::open_wait(&paths, std::time::Duration::from_millis(80)).await;
    assert_eq!(busy.err().unwrap().0, "adapter_busy");
    drop(waited);
    release.join().unwrap();
}

/// A slow native add must not hold the ledger lease: concurrent receipt reads
/// proceed, a second sender of the same id waits and then submits nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_delivery_releases_lease_and_one_id_is_delivered_once() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    TEST_WAIT
        .lock()
        .unwrap()
        .push((paths.root.clone(), std::time::Duration::from_secs(10)));
    let adds = Arc::new(Mutex::new(0usize));
    let lease_free_during_add = Arc::new(Mutex::new(None::<(bool, String)>));
    let (counter, probe, p, message) = (
        adds.clone(),
        lease_free_during_add.clone(),
        paths.clone(),
        input.message_id.clone(),
    );
    let server = scripted_connections(
        &a.endpoint(),
        move |req| match req["method"].as_str().unwrap() {
            "thread/read" => {
                Some(json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"idle"}}}))
            }
            "thread/queue/add" => {
                *counter.lock().unwrap() += 1;
                let state = Ledger::open(&p)
                    .ok()
                    .and_then(|db| db.get(&message).ok().flatten())
                    .map(|r| r.state);
                *probe.lock().unwrap() = Some((state.is_some(), state.unwrap_or_default()));
                std::thread::sleep(std::time::Duration::from_millis(600));
                Some(
                    json!({"queuedSubmission":{"id":"q1","clientUserMessageId":req["params"]["clientUserMessageId"]}}),
                )
            }
            other => panic!("unexpected {other}"),
        },
        2,
    )
    .await;
    input.expected_generation = a.generation();
    let first = {
        let (paths, input) = (paths.clone(), input.clone());
        tokio::spawn(async move {
            let mut db = Ledger::open(&paths).unwrap();
            let mut receipt = db.accept(&input, now).unwrap();
            deliver(&paths, &mut db, &mut receipt, Some(&input))
                .await
                .unwrap();
            receipt
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    // A receipt read while the add is outstanding is not blocked and sees the journal.
    let started = std::time::Instant::now();
    let mut reader = Ledger::open_wait(&paths, std::time::Duration::from_secs(5))
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_millis(300));
    let mut seen = reader.lookup(&input.message_id).unwrap();
    assert_eq!(seen.state, "unknown");
    // A second sender of the same id waits for session ownership, then finds the
    // first process's result and does not submit again.
    let second = {
        let (paths, input) = (paths.clone(), input.clone());
        tokio::spawn(async move {
            let mut db = Ledger::open_wait(&paths, std::time::Duration::from_secs(10))
                .await
                .unwrap();
            let mut receipt = db.accept(&input, now).unwrap();
            // Normally still `unknown`; under heavy load the first may already be done.
            assert!(matches!(
                receipt.state.as_str(),
                "unknown" | "native_accepted"
            ));
            deliver(&paths, &mut db, &mut receipt, Some(&input))
                .await
                .unwrap();
            receipt
        })
    };
    // Status-style observation of the in-flight receipt must not regress anything.
    reader.expire(&mut seen, crate::daemon::now()).unwrap();
    drop(reader);
    let first = first.await.unwrap();
    let second = second.await.unwrap();
    assert_eq!(first.state, "native_accepted");
    assert_eq!(second.state, "native_accepted");
    assert_eq!(*adds.lock().unwrap(), 1);
    assert_eq!(
        lease_free_during_add.lock().unwrap().clone(),
        Some((true, "unknown".to_string()))
    );
    server.abort();
}

/// Receipt observation reads Codex without the ledger lease and never regresses a
/// state another process recorded meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observation_io_is_lease_free_and_does_not_regress_terminal_state() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    TEST_WAIT
        .lock()
        .unwrap()
        .push((paths.root.clone(), std::time::Duration::from_secs(10)));
    let p = paths.clone();
    let message = input.message_id.clone();
    let free = Arc::new(Mutex::new(false));
    let seen_free = free.clone();
    let server = scripted_connections(
        &a.endpoint(),
        move |req| {
            assert_eq!(req["method"], "thread/turns/list");
            // Another process records a terminal state while the read is outstanding.
            let mut db = Ledger::open(&p).unwrap();
            *seen_free.lock().unwrap() = true;
            let mut r = db.get(&message).unwrap().unwrap();
            r.state = "failed".into();
            r.reason = Some("native_turn_failed".into());
            db.save(&r).unwrap();
            Some(json!({"data":[],"nextCursor":null}))
        },
        1,
    )
    .await;
    input.expected_generation = a.generation();
    let mut db = Ledger::open(&paths).unwrap();
    let mut receipt = db.accept(&input, now).unwrap();
    receipt.state = "native_accepted".into();
    receipt.native_queue_id = Some("q1".into());
    db.save(&receipt).unwrap();
    let out = observe::result(&paths, &mut db, &mut receipt)
        .await
        .unwrap();
    assert!(*free.lock().unwrap());
    assert_eq!(out["state"], "failed");
    assert_eq!(
        out["observation"]["code"],
        "receipt_changed_during_observation"
    );
    assert_eq!(db.get(&input.message_id).unwrap().unwrap().state, "failed");
    server.abort();
}

#[tokio::test]
async fn if_busy_is_validated_and_only_valid_with_submit() {
    let (_dir, paths, input, _) = fixture();
    for (mode, value) in [
        (PublicMode::Queue, "queue"),
        (PublicMode::Submit, "steer"),
        (PublicMode::Steer, "queue"),
    ] {
        let request = PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: Some(value.into()),
            steering: None,
            recovery: None,
            api_version: 1,
            message_id: input.message_id.clone(),
            session_id: input.session_id.clone(),
            expected_generation: input.expected_generation.clone(),
            mode,
            text: "hello".into(),
            expires_at: input.expires_at,
        };
        let error = send_public(&paths, request).await.unwrap_err().to_string();
        assert!(error.contains("INVALID_REQUEST: if_busy"), "{error}");
    }
}

#[test]
fn a_stale_error_never_overwrites_a_receipt_another_sender_settled() {
    let (_dir, paths, input, now) = fixture();
    let mut db = Ledger::open(&paths).unwrap();
    let mut stale = db.accept(&input, now).unwrap();
    let mut settled = stale.clone();
    settled.state = "native_accepted".into();
    settled.native_queue_id = Some("q1".into());
    db.save(&settled).unwrap();
    db.amend(&mut stale, |r| {
        r.state = "rejected".into();
        r.reason = Some("target_busy".into());
    })
    .unwrap();
    assert_eq!(stale, settled, "the stored record is adopted");
    assert_eq!(db.get(&input.message_id).unwrap().unwrap(), settled);
    // Unchanged receipts are amended and saved.
    db.amend(&mut stale, |r| r.reason = Some("note".into()))
        .unwrap();
    assert_eq!(
        db.get(&input.message_id)
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("note")
    );
}
