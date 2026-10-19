use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
fn call(root: &Path, args: &[&str], input: Option<&Value>) -> (bool, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sessionsation"))
        .env("CODEX_HOME", root.join("disposable-codex-home"))
        .arg("--data-dir")
        .arg(root)
        .arg("message")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.stderr.is_empty(), "unexpected diagnostics");
    (
        output.status.success(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}
#[test]
fn shared_contract_cli_offline_round_trip_and_structured_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let fixtures: Value =
        serde_json::from_str(include_str!("../docs/messaging/v1.2.1/fixtures.json")).unwrap();
    let (ok, caps) = call(&root, &["capabilities"], None);
    assert!(ok);
    assert_eq!(caps["contract_version"], fixtures["contract_version"]);
    assert_eq!(
        caps["result"]["events"]["public_replies"],
        "correlated_terminal_summary"
    );
    let (_, sessions) = call(&root, &["sessions"], None);
    assert_eq!(sessions["result"]["sessions"], json!([]));
    let (_, bad) = call(
        &root,
        &["resolve", "--session", "ambiguous-display-name"],
        None,
    );
    assert_eq!(bad["error"]["code"], "invalid_id");
    let mut input = fixtures["enqueue"].clone();
    input["expires_at"] = json!(sessionsation::daemon::now() + 300);
    let id = input["message_id"].as_str().unwrap();
    let session = input["session_id"].as_str().unwrap();
    let (_, resolved) = call(&root, &["resolve", "--session", session], None);
    assert_eq!(resolved["result"]["availability"], "unavailable");
    assert!(
        resolved["result"]
            .as_object()
            .unwrap()
            .contains_key("display_name")
    );
    assert!(resolved["result"]["display_name"].is_null());
    assert!(resolved["result"]["account_name"].is_null());
    let (ok, accepted) = call(&root, &["enqueue"], Some(&input));
    assert!(ok);
    assert_eq!(accepted["result"]["state"], "pending_local");
    assert_eq!(accepted["result"]["reason"], "unavailable");
    let (_, retried) = call(&root, &["enqueue"], Some(&input));
    assert_eq!(retried, accepted);
    let (_, status) = call(&root, &["status", "--message", id], None);
    assert_eq!(status["result"]["state"], accepted["result"]["state"]);
    assert_eq!(status["result"]["observation"]["state"], "not_applicable");
    let mut conflict = input.clone();
    conflict["text"] = json!("different");
    let (ok, error) = call(&root, &["enqueue"], Some(&conflict));
    assert!(!ok);
    assert_eq!(error["error"]["code"], "message_id_conflict");
    let (ok, cancelled) = call(&root, &["cancel", "--message", id], None);
    assert!(ok);
    assert_eq!(cancelled["result"]["state"], "cancelled");
    let (_, events) = call(&root, &["events"], None);
    let cursor = events["result"]["cursor"].as_str().unwrap();
    let (_, tail) = call(&root, &["events", "--cursor", cursor], None);
    assert_eq!(tail["result"]["events"], json!([]));
    assert!(!events.to_string().contains(input["text"].as_str().unwrap()));
    assert!(!root.join("registry.db").exists());
    assert!(!root.join("config.json").exists());
    assert!(!root.join("accounts").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correlated_reply_survives_real_cli_process_retries_and_restarts() {
    use futures_util::{SinkExt, StreamExt};
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio_tungstenite::tungstenite::Message;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let home = tmp.path().join("fake-home");
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    fs::create_dir_all(root.join("managed-panes")).unwrap();
    let fixtures: Value =
        serde_json::from_str(include_str!("../docs/messaging/v1.2.1/fixtures.json")).unwrap();
    let mut input = fixtures["enqueue"].clone();
    input["expires_at"] = json!(sessionsation::daemon::now() + 300);
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    fs::write(
        root.join("managed-panes/1.json"),
        json!({"pid":pid,"start_time":start,"home":home,"thread":input["session_id"]}).to_string(),
    )
    .unwrap();
    let listener =
        tokio::net::UnixListener::bind(home.join("app-server-control/app-server-control.sock"))
            .unwrap();
    let adds = Arc::new(AtomicUsize::new(0));
    let count = adds.clone();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let reply = match request["method"].as_str().unwrap() {
                    "initialize" => json!({"userAgent":"fake/0.163.0"}),
                    "initialized" => continue,
                    "thread/read" => {
                        json!({"thread":{"id":request["params"]["threadId"],"status":{"type":"active"}}})
                    }
                    "thread/queue/add" => {
                        count.fetch_add(1, Ordering::SeqCst);
                        break;
                    } // accepted, acknowledgement lost
                    "thread/queue/list" => json!({"data":[],"nextCursor":null}),
                    "thread/turns/list" => {
                        assert_eq!(request["params"]["itemsView"], "summary");
                        fixtures["native_summary"].clone()
                    }
                    other => panic!("forbidden native method {other}"),
                };
                socket.send(Message::Text(json!({"method":"item/reasoning/textDelta","params":{"delta":"DO_NOT_EXPORT"}}).to_string().into())).await.unwrap();
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":reply})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        }
    });
    let (_, resolved) = call(
        &root,
        &[
            "resolve",
            "--session",
            input["session_id"].as_str().unwrap(),
        ],
        None,
    );
    fs::create_dir_all(root.join("conversations")).unwrap();
    fs::write(
        root.join("conversations")
            .join(format!("{}.json", input["session_id"].as_str().unwrap())),
        json!({"name":"研究 café"}).to_string(),
    )
    .unwrap();
    let (ok, listed) = call(&root, &["sessions"], None);
    assert!(ok);
    assert_eq!(listed["result"]["sessions"][0]["display_name"], "研究 café");
    assert!(listed["result"]["sessions"][0]["account_name"].is_null());
    assert_eq!(
        listed["result"]["sessions"][0]["generation"],
        resolved["result"]["generation"]
    );
    input["expected_generation"] = resolved["result"]["generation"].clone();
    let (ok, first) = call(&root, &["enqueue"], Some(&input));
    assert!(ok);
    assert_eq!(first["result"]["state"], "unknown");
    let (ok, retry) = call(&root, &["enqueue"], Some(&input));
    assert!(ok);
    assert_eq!(retry["result"]["state"], "unknown");
    let id = input["message_id"].as_str().unwrap();
    let (ok, status) = call(&root, &["observe", "--message", id], None);
    assert!(ok);
    assert_eq!(status["result"]["state"], "completed_with_reply");
    assert_eq!(status["result"]["reply"]["text"], "Visible answer.");
    assert_eq!(adds.load(Ordering::SeqCst), 1);
    task.abort();
    let (_, cached) = call(&root, &["status", "--message", id], None);
    assert_eq!(cached["result"]["observation"]["state"], "cached");
    assert_eq!(cached["result"]["reply"], status["result"]["reply"]);
    let (_, events) = call(&root, &["events"], None);
    let rows = events["result"]["events"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|e| e["receipt"]["state"] == "completed_with_reply")
            .count(),
        1
    );
    assert!(!events.to_string().contains("DO_NOT_EXPORT"));
    let (_, replayed) = call(&root, &["events"], None);
    assert_eq!(events, replayed);
}

#[test]
fn modes_fixture_is_isolated_and_busy_submit_is_supported() {
    let tmp = tempfile::tempdir().unwrap();
    let bundle: Value = serde_json::from_str(include_str!("fixtures/legacy-modes.json")).unwrap();
    let (ok, cap) = call(tmp.path(), &["modes", "capabilities"], None);
    assert!(ok);
    assert_eq!(cap["contract_version"], "1.4.0");
    assert_eq!(cap["result"]["native_sending_enabled"], true);
    assert_eq!(cap["result"]["modes"]["submit"]["live"], true);
    let (ok, result) = call(tmp.path(), &["modes", "fixture"], Some(&bundle["scenario"]));
    assert!(ok);
    assert_eq!(
        result["result"]["isolation"],
        "temporary_sqlite_no_native_io"
    );
    assert_eq!(
        result["result"]["results"][1]["result"]["mock_call"]["method"],
        "turn/start"
    );
    let (ok, denied) = call(tmp.path(), &["modes", "request"], Some(&bundle["request"]));
    assert!(!ok);
    assert_eq!(denied["error"]["code"], "target_unavailable");
    let (_, legacy) = call(tmp.path(), &["capabilities"], None);
    assert_eq!(legacy["contract_version"], "1.2.1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modes_discover_two_managed_targets_without_registration_and_fail_closed() {
    use futures_util::{SinkExt, StreamExt};
    use std::{fs, os::unix::fs::PermissionsExt};
    use tokio_tungstenite::tungstenite::Message;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    fs::create_dir_all(root.join("managed-panes")).unwrap();
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let ids = [
        "11111111-1111-4111-8111-111111111111",
        "22222222-2222-4222-8222-222222222222",
    ];
    let mismatch = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let race = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut tasks = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let home = tmp.path().join(format!("home-{i}"));
        fs::create_dir_all(home.join("app-server-control")).unwrap();
        fs::write(
            root.join(format!("managed-panes/{i}.json")),
            json!({"pid":pid,"start_time":start,"home":home,"thread":id}).to_string(),
        )
        .unwrap();
        let listener =
            tokio::net::UnixListener::bind(home.join("app-server-control/app-server-control.sock"))
                .unwrap();
        let mismatch = mismatch.clone();
        let race = race.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let mut reads = 0;
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    let result = match req["method"].as_str().unwrap() {
                        "initialize" => json!({"userAgent":"fixture/0.163.0"}),
                        "initialized" => continue,
                        "thread/read" => { reads += 1; json!({"thread":{"id":if mismatch.load(std::sync::atomic::Ordering::SeqCst) {json!("other-thread")} else {req["params"]["threadId"].clone()},"status":{"type":if i==0 && !(race.load(std::sync::atomic::Ordering::SeqCst) && reads>1) {"idle"}else{"active"}}}}) },
                        "turn/start" => json!({"turn":{"id":"new-turn","status":"inProgress"}}),
                        "thread/turns/list" => json!({"data":if i==0 {vec![]} else {vec![json!({"id":"turn-a","status":"inProgress"})]},"nextCursor":null}),
                        other => panic!("unexpected native operation {other}"),
                    };
                    socket.send(Message::Text(json!({"id":req["id"],"result":result}).to_string().into())).await.unwrap();
                }
            }
        }));
    }
    // An obsolete policy file cannot secretly restrict discovery or routing.
    fs::create_dir_all(root.join("messaging")).unwrap();
    fs::write(
        root.join("messaging/modes-policy.json"),
        "{\"allowlisted_sessions\":[]}",
    )
    .unwrap();
    let (ok, listed) = call(&root, &["modes", "sessions"], None);
    assert!(ok);
    assert_eq!(listed["result"]["sessions"].as_array().unwrap().len(), 2);
    let fixtures: Value = serde_json::from_str(include_str!("fixtures/legacy-modes.json")).unwrap();
    for (i, id) in ids.iter().enumerate() {
        let (ok, target) = call(&root, &["modes", "target", "--session", id], None);
        assert!(ok);
        assert_eq!(
            target["result"]["state"],
            if i == 0 { "idle" } else { "working" }
        );
        let mut request = fixtures["request"].clone();
        request["session_id"] = json!(id);
        request["request_id"] = json!(id);
        request["expected_generation"] = target["result"]["generation"].clone();
        request["expected_turn_id"] = if i == 0 { Value::Null } else { json!("turn-a") };
        if i == 0 {
            request["capability_id"] = target["result"]["submit_capability_id"].clone();
            request["created_at"] = json!(sessionsation::daemon::now());
            request["expires_at"] = json!(sessionsation::daemon::now() + 300);
        }
        let (_, denied) = call(&root, &["modes", "request"], Some(&request));
        if i == 0 {
            assert_eq!(denied["result"]["state"], "native_accepted");
        } else {
            assert_eq!(denied["error"]["code"], "capability_changed");
        }
        request["request_id"] = json!(format!("00000000-0000-4000-8000-{:012}", i + 10));
        request["mode"] = json!("queue");
        request["expected_turn_id"] = Value::Null;
        request["capability_id"] = target["result"]["capability_id"].clone();
        request["created_at"] = json!(sessionsation::daemon::now());
        request["expires_at"] = json!(sessionsation::daemon::now() + 300);
        let (_, disabled) = call(&root, &["modes", "sending", "--disable"], None);
        assert_eq!(disabled["result"]["native_sending_enabled"], false);
        let (_, denied) = call(&root, &["modes", "request"], Some(&request));
        assert_eq!(denied["error"]["code"], "native_sending_disabled");
        call(&root, &["modes", "sending", "--enable"], None);
        request["expected_generation"] = json!("f".repeat(64));
        let (_, denied) = call(&root, &["modes", "request"], Some(&request));
        assert_eq!(denied["error"]["code"], "capability_changed");
    }
    let (_, prepared) = call(
        &root,
        &[
            "modes",
            "submit-check",
            "--session",
            ids[1],
            "--expected-turn",
            "turn-a",
        ],
        None,
    );
    assert_eq!(prepared["result"]["candidate_action"], "steer");
    assert_eq!(prepared["result"]["dispatch_supported"], true);
    let (_, conflict) = call(
        &root,
        &[
            "modes",
            "submit-check",
            "--session",
            ids[1],
            "--expected-turn",
            "old-turn",
        ],
        None,
    );
    assert_eq!(conflict["error"]["code"], "turn_conflict");
    race.store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, race_result) = call(&root, &["modes", "submit-check", "--session", ids[0]], None);
    assert_eq!(race_result["error"]["code"], "state_changed");
    race.store(false, std::sync::atomic::Ordering::SeqCst);
    let (_, unknown) = call(
        &root,
        &[
            "modes",
            "target",
            "--session",
            "33333333-3333-4333-8333-333333333333",
        ],
        None,
    );
    assert_eq!(unknown["error"]["code"], "target_unavailable");
    mismatch.store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, mismatched) = call(&root, &["modes", "target", "--session", ids[1]], None);
    assert_eq!(mismatched["result"]["reason"], "correlation_changed");
    mismatch.store(false, std::sync::atomic::Ordering::SeqCst);
    let (_, old) = call(&root, &["modes", "target", "--session", ids[0]], None);
    let binding = root.join("managed-panes/0.json");
    let original = fs::read(&binding).unwrap();
    let mut moved: Value = serde_json::from_slice(&original).unwrap();
    moved["home"] = json!(tmp.path().join("home-1"));
    fs::write(&binding, moved.to_string()).unwrap();
    let (_, new) = call(&root, &["modes", "target", "--session", ids[0]], None);
    assert_eq!(new["result"]["session_id"], old["result"]["session_id"]);
    assert_ne!(new["result"]["generation"], old["result"]["generation"]);
    let mut request = fixtures["request"].clone();
    request["session_id"] = json!(ids[0]);
    request["request_id"] = json!("99999999-9999-4999-8999-999999999999");
    request["expected_generation"] = old["result"]["generation"].clone();
    let (_, swapped) = call(&root, &["modes", "request"], Some(&request));
    assert_eq!(swapped["error"]["code"], "generation_mismatch");
    fs::write(root.join("managed-panes/duplicate.json"), &original).unwrap();
    let (_, ambiguous) = call(&root, &["modes", "target", "--session", ids[0]], None);
    assert_eq!(ambiguous["error"]["code"], "ambiguous_session");
    fs::remove_file(root.join("managed-panes/duplicate.json")).unwrap();
    fs::write(&binding, original).unwrap();
    fs::set_permissions(
        tmp.path().join("home-0/app-server-control"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let (_, unsafe_target) = call(&root, &["modes", "target", "--session", ids[0]], None);
    assert_eq!(unsafe_target["result"]["state"], "unknown");
    assert_eq!(unsafe_target["result"]["reason"], "unsafe_endpoint");
    for task in tasks {
        task.abort();
    }
    let output = listed.to_string();
    assert!(!output.contains(tmp.path().to_str().unwrap()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_modes_queue_roundtrip_fifo_lost_ack_retry_cancel_and_emergency_gate() {
    use futures_util::{SinkExt, StreamExt};
    use std::{
        fs,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };
    use tokio_tungstenite::tungstenite::Message;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let home = tmp.path().join("home");
    fs::create_dir_all(root.join("managed-panes")).unwrap();
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let session = "22222222-2222-4222-8222-222222222222";
    fs::write(
        root.join("managed-panes/1.json"),
        json!({"pid":pid,"start_time":start,"home":home,"thread":session}).to_string(),
    )
    .unwrap();
    let listener =
        tokio::net::UnixListener::bind(home.join("app-server-control/app-server-control.sock"))
            .unwrap();
    let adds = Arc::new(AtomicUsize::new(0));
    let lost = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));
    let native_busy = busy.clone();
    let rows = Arc::new(Mutex::new(Vec::<String>::new()));
    let (count, drop_ack, completed, native_rows) =
        (adds.clone(), lost.clone(), done.clone(), rows.clone());
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let result = match req["method"].as_str().unwrap() {
                    "initialize" => json!({"userAgent":"fake/0.163.0"}),
                    "initialized" => continue,
                    "thread/read" => {
                        json!({"thread":{"id":session,"status":{"type":if native_busy.load(Ordering::SeqCst) {"active"} else {"idle"}}}})
                    }
                    "thread/queue/add" => {
                        let id = req["params"]["clientUserMessageId"]
                            .as_str()
                            .unwrap()
                            .to_owned();
                        count.fetch_add(1, Ordering::SeqCst);
                        native_rows.lock().unwrap().push(id.clone());
                        if drop_ack.swap(false, Ordering::SeqCst) {
                            break;
                        }
                        json!({"queuedSubmission":{"id":format!("q-{id}"),"clientUserMessageId":id}})
                    }
                    "thread/queue/list" => {
                        json!({"data":if completed.load(Ordering::SeqCst) {vec![]} else {native_rows.lock().unwrap().iter().map(|id| json!({"id":format!("q-{id}"),"clientUserMessageId":id})).collect::<Vec<_>>()},"nextCursor":null})
                    }
                    "thread/queue/delete" => {
                        let id = req["params"]["queuedSubmissionId"]
                            .as_str()
                            .unwrap()
                            .strip_prefix("q-")
                            .unwrap();
                        let mut rows = native_rows.lock().unwrap();
                        let len = rows.len();
                        rows.retain(|r| r != id);
                        json!({"deleted":rows.len()!=len})
                    }
                    "thread/turns/list" => {
                        assert_eq!(req["params"]["itemsView"], "summary");
                        json!({"data":if !completed.load(Ordering::SeqCst) {vec![]} else {native_rows.lock().unwrap().iter().map(|id| json!({"id":format!("turn-{id}"),"status":"completed","itemsView":"summary","items":[{"type":"userMessage","id":"item-1","clientId":id},{"type":"agentMessage","id":"item-2","phase":"final_answer","text":"Synthetic queue reply"}]})).collect::<Vec<_>>()},"nextCursor":null})
                    }
                    method => panic!("forbidden native operation {method}"),
                };
                ws.send(Message::Text(
                    json!({"id":req["id"],"result":result}).to_string().into(),
                ))
                .await
                .unwrap();
            }
        }
    });
    let (_, target) = call(&root, &["modes", "target", "--session", session], None);
    assert_eq!(target["result"]["state"], "idle");
    let make = |id: &str| json!({"request_id":id,"session_id":session,"mode":"queue","text":"Synthetic queue input","expected_generation":target["result"]["generation"],"expected_turn_id":null,"capability_id":target["result"]["capability_id"],"created_at":sessionsation::daemon::now(),"expires_at":sessionsation::daemon::now()+300});
    let first = make("11111111-1111-4111-8111-111111111111");
    let second = make("33333333-3333-4333-8333-333333333333");
    let third = make("44444444-4444-4444-8444-444444444444");
    let fourth = make("55555555-5555-4555-8555-555555555555");
    let (ok, accepted) = call(&root, &["modes", "request"], Some(&first));
    assert!(ok, "{accepted}");
    assert_eq!(accepted["result"]["state"], "native_accepted");
    // Already-idle Queue reaches native insertion without a future completion event.
    assert_eq!(adds.load(Ordering::SeqCst), 1);
    assert!(!done.load(Ordering::SeqCst));
    assert_eq!(accepted["result"]["requested_mode"], "queue");
    assert_eq!(accepted["result"]["effective_action"], "queue");
    assert_eq!(accepted["result"]["facts"]["input_consumed"], "unknown");
    assert_eq!(call(&root, &["modes", "request"], Some(&first)).1, accepted);
    let mut changed = first.clone();
    changed["mode"] = json!("submit");
    assert_eq!(
        call(&root, &["modes", "request"], Some(&changed)).1["error"]["code"],
        "idempotency_conflict"
    );
    busy.store(true, Ordering::SeqCst);
    lost.store(true, Ordering::SeqCst);
    let (_, uncertain) = call(&root, &["modes", "request"], Some(&second));
    assert_eq!(uncertain["result"]["state"], "delivery_unknown");
    assert_eq!(
        call(&root, &["modes", "request"], Some(&second)).1,
        uncertain
    );
    assert_eq!(adds.load(Ordering::SeqCst), 2);
    assert_eq!(
        call(&root, &["modes", "request"], Some(&third)).1["error"]["code"],
        "prior_delivery_unknown"
    );
    let second_id = second["request_id"].as_str().unwrap();
    let (_, reconciled) = call(&root, &["modes", "status", "--request", second_id], None);
    assert_eq!(reconciled["result"]["state"], "native_accepted");
    assert!(call(&root, &["modes", "request"], Some(&third)).0);
    assert_eq!(
        *rows.lock().unwrap(),
        vec![
            first["request_id"].as_str().unwrap(),
            second_id,
            third["request_id"].as_str().unwrap()
        ]
    );
    call(&root, &["modes", "sending", "--disable"], None);
    assert_eq!(
        call(&root, &["modes", "request"], Some(&fourth)).1["error"]["code"],
        "native_sending_disabled"
    );
    // Emergency disable also blocks the existing trusted-local legacy enqueue surface.
    let legacy = json!({"message_id":fourth["request_id"],"session_id":session,"text":"Synthetic legacy input","expected_generation":target["result"]["generation"],"expires_at":sessionsation::daemon::now()+300});
    assert_eq!(
        call(&root, &["enqueue"], Some(&legacy)).1["result"]["reason"],
        "native_sending_disabled"
    );
    assert_eq!(adds.load(Ordering::SeqCst), 3);
    let third_id = third["request_id"].as_str().unwrap();
    assert_eq!(
        call(&root, &["modes", "cancel", "--request", third_id], None).1["result"]["state"],
        "cancelled"
    );
    call(&root, &["modes", "sending", "--enable"], None);
    done.store(true, Ordering::SeqCst);
    let (ok, completed) = call(&root, &["modes", "status", "--request", second_id], None);
    assert!(ok);
    assert_eq!(completed["result"]["state"], "completed_with_reply");
    assert_eq!(
        completed["result"]["delivery"]["reply"]["text"],
        "Synthetic queue reply"
    );
    assert_eq!(
        completed["result"]["delivery"]["correlation"]["user_item_id"],
        "item-1"
    );
    let (_, events) = call(&root, &["modes", "events"], None);
    assert!(!events.to_string().contains("Synthetic queue input"));
    assert!(
        !events
            .to_string()
            .contains(fourth["request_id"].as_str().unwrap())
    );
    let cursor = events["result"]["cursor"].as_str().unwrap();
    assert_eq!(
        call(&root, &["modes", "events", "--cursor", cursor], None).1["result"]["events"],
        json!([])
    );
    task.abort();
    let (_, cached) = call(&root, &["modes", "status", "--request", second_id], None);
    assert_eq!(
        cached["result"]["delivery"]["observation"]["state"],
        "cached"
    );
    assert_eq!(
        cached["result"]["delivery"]["reply"],
        completed["result"]["delivery"]["reply"]
    );
    assert_eq!(adds.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_submit_native_guard_uncertainty_and_restart_never_resend_or_claim_replies() {
    use futures_util::{SinkExt, StreamExt};
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio_tungstenite::tungstenite::Message;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let home = tmp.path().join("home");
    fs::create_dir_all(root.join("managed-panes")).unwrap();
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let session = "22222222-2222-4222-8222-222222222222";
    fs::write(
        root.join("managed-panes/1.json"),
        json!({"pid":pid,"start_time":start,"home":home,"thread":session}).to_string(),
    )
    .unwrap();
    let listener =
        tokio::net::UnixListener::bind(home.join("app-server-control/app-server-control.sock"))
            .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let behavior = Arc::new(AtomicUsize::new(0));
    let (count, mode) = (calls.clone(), behavior.clone());
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let q: Value = serde_json::from_str(&text).unwrap();
                let result = match q["method"].as_str().unwrap() {
                    "initialize" => json!({"userAgent":"fixture/0.163.0"}),
                    "initialized" => continue,
                    "thread/read" => json!({"thread":{"id":session,"status":{"type":"active"}}}),
                    "thread/turns/list" => {
                        json!({"data":[{"id":"turn-a","status":"inProgress","items":[{"type":"agentMessage","text":"PRIOR_REPLY_NOT_FOR_THIS_REQUEST"}]}],"nextCursor":null})
                    }
                    "turn/steer" => {
                        count.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(q["params"]["threadId"], session);
                        assert_eq!(q["params"]["expectedTurnId"], "turn-a");
                        assert!(q["params"]["clientUserMessageId"].is_string());
                        assert_eq!(
                            q["params"]["input"][0]["text"],
                            "literal $(not-a-shell) input"
                        );
                        let behavior = mode.load(Ordering::SeqCst);
                        if behavior == 1 {
                            break;
                        } // Accepted but response lost.
                        if behavior == 2 {
                            // Human changed/completed the turn at the native boundary.
                            ws.send(Message::Text(json!({"id":q["id"],"error":{"code":-32600,"message":"PRIVATE_NATIVE_ERROR"}}).to_string().into())).await.unwrap();
                            continue;
                        }
                        ws.send(Message::Text(json!({"method":"item/completed","params":{"item":{"type":"reasoning","text":"PRIVATE_REASONING"}}}).to_string().into())).await.unwrap();
                        json!({"turnId":if behavior==3 {"wrong-turn"} else {"turn-a"}})
                    }
                    method => panic!("unexpected mutation or fallback {method}"),
                };
                ws.send(Message::Text(
                    json!({"id":q["id"],"result":result}).to_string().into(),
                ))
                .await
                .unwrap();
            }
        }
    });
    let (_, target) = call(&root, &["modes", "target", "--session", session], None);
    let (_, discovered_turn) = call(
        &root,
        &["modes", "submit-check", "--session", session],
        None,
    );
    assert_eq!(discovered_turn["result"]["active_turn_id"], "turn-a");
    let now = sessionsation::daemon::now();
    let mut request = json!({"request_id":"11111111-1111-4111-8111-111111111111","session_id":session,"mode":"submit","text":"literal $(not-a-shell) input","expected_generation":target["result"]["generation"],"expected_turn_id":"turn-a","capability_id":target["result"]["submit_capability_id"],"created_at":now,"expires_at":now+300});
    let mut missing_turn = request.clone();
    missing_turn["expected_turn_id"] = Value::Null;
    let (_, missing) = call(&root, &["modes", "request"], Some(&missing_turn));
    assert_eq!(missing["error"]["code"], "invalid_precondition");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let (ok, accepted) = call(&root, &["modes", "request"], Some(&request));
    assert!(ok, "{accepted}");
    assert_eq!(accepted["result"]["state"], "native_accepted");
    assert_eq!(accepted["result"]["effective_action"], "steer");
    assert_eq!(accepted["result"]["facts"]["input_consumed"], "unknown");
    assert!(accepted["result"]["reply"].is_null());
    let (_, retry) = call(&root, &["modes", "request"], Some(&request));
    assert_eq!(accepted, retry);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for field in ["mode", "expected_turn_id", "text", "session_id"] {
        let mut conflict = request.clone();
        conflict[field] = json!(match field {
            "mode" => "queue",
            "session_id" => "33333333-3333-4333-8333-333333333333",
            _ => "different",
        });
        if field == "mode" {
            conflict["expected_turn_id"] = Value::Null;
        }
        let (_, error) = call(&root, &["modes", "request"], Some(&conflict));
        assert_eq!(error["error"]["code"], "idempotency_conflict");
    }
    let id = request["request_id"].as_str().unwrap();
    let (_, cancel) = call(&root, &["modes", "cancel", "--request", id], None);
    assert_eq!(cancel["error"]["code"], "already_submitted");
    let (_, legacy) = call(&root, &["status", "--message", id], None);
    assert_eq!(
        legacy["error"]["code"],
        "mode_receipt_requires_modes_status"
    );
    let (_, legacy_events) = call(&root, &["events"], None);
    assert_eq!(legacy_events["result"]["events"], json!([]));
    call(&root, &["modes", "sending", "--disable"], None);
    request["request_id"] = json!("44444444-4444-4444-8444-444444444444");
    let (_, disabled) = call(&root, &["modes", "request"], Some(&request));
    assert_eq!(disabled["error"]["code"], "native_sending_disabled");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    call(&root, &["modes", "sending", "--enable"], None);
    let mut unknowns = Vec::new();
    for (i, id) in [
        (1, "44444444-4444-4444-8444-444444444444"),
        (2, "55555555-5555-4555-8555-555555555555"),
        (3, "66666666-6666-4666-8666-666666666666"),
    ] {
        behavior.store(i, Ordering::SeqCst);
        request["request_id"] = json!(id);
        let (ok, out) = call(&root, &["modes", "request"], Some(&request));
        assert!(ok);
        assert_eq!(out["result"]["state"], "delivery_unknown");
        assert!(out["result"]["effective_action"].is_null());
        let (_, repeat) = call(&root, &["modes", "request"], Some(&request));
        assert_eq!(out, repeat);
        let (_, cancel) = call(&root, &["modes", "cancel", "--request", id], None);
        assert_eq!(cancel["error"]["code"], "unknown");
        unknowns.push(request.clone());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    task.abort(); // Restart/status remains useful with no endpoint; never polls unrelated output.
    let (_, status) = call(
        &root,
        &[
            "modes",
            "status",
            "--request",
            "11111111-1111-4111-8111-111111111111",
        ],
        None,
    );
    assert_eq!(status, accepted);
    // Simulate passing submission deadlines in disposable ledger only.
    let db = rusqlite::Connection::open(root.join("messaging/receipts.db")).unwrap();
    db.execute("UPDATE receipts SET expiry=0", []).unwrap();
    drop(db);
    for r in unknowns {
        let (_, out) = call(&root, &["modes", "request"], Some(&r));
        assert_eq!(out["result"]["state"], "delivery_unknown");
        let (_, status) = call(
            &root,
            &[
                "modes",
                "status",
                "--request",
                r["request_id"].as_str().unwrap(),
            ],
            None,
        );
        assert_eq!(out, status);
    }
    let (_, events) = call(&root, &["modes", "events"], None);
    let text = events.to_string();
    for forbidden in [
        "PRIVATE_REASONING",
        "PRIVATE_NATIVE_ERROR",
        "PRIOR_REPLY_NOT_FOR_THIS_REQUEST",
        "literal $(not-a-shell) input",
    ] {
        assert!(!text.contains(forbidden));
    }
    let cursor = events["result"]["cursor"].as_str().unwrap();
    let (_, tail) = call(&root, &["modes", "events", "--cursor", cursor], None);
    assert_eq!(tail["result"]["events"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_submit_resume_and_read_only_states_survive_cli_restart() {
    use futures_util::{SinkExt, StreamExt};
    use std::{
        fs,
        sync::{Arc, Mutex},
    };
    use tokio_tungstenite::tungstenite::Message;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let home = tmp.path().join("fixture-home");
    fs::create_dir_all(root.join("managed-panes")).unwrap();
    fs::create_dir_all(home.join("app-server-control")).unwrap();
    let id = "11111111-1111-4111-8111-111111111111";
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    fs::write(
        root.join("managed-panes/1.json"),
        json!({"thread":id,"home":home,"pid":pid,"start_time":start}).to_string(),
    )
    .unwrap();
    // (state, previous turn, accepted inputs, lose ack, race at native reservation)
    let state = Arc::new(Mutex::new((
        "idle".to_string(),
        "previous".to_string(),
        0usize,
        false,
        false,
    )));
    let fixture = state.clone();
    let listener =
        tokio::net::UnixListener::bind(home.join("app-server-control/app-server-control.sock"))
            .unwrap();
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let result = {
                    let mut s = fixture.lock().unwrap();
                    match req["method"].as_str().unwrap() {
                        "initialized" => continue,
                        "initialize" => json!({"userAgent":"fixture/0.163.0"}),
                        "thread/read" => {
                            json!({"thread":{"id":id,"status":{"type":if s.0=="working" {"active"}else{"idle"}}}})
                        }
                        "thread/turns/list" => {
                            assert_eq!(req["params"]["itemsView"], "summary");
                            json!({"data":[{"id":s.1,"status":match s.0.as_str(){"interrupted"=>"interrupted","errored"=>"failed","working"=>"inProgress",_=>"completed"},"items":[{"type":"reasoning","text":"DO_NOT_EXPORT"}]}],"nextCursor":null})
                        }
                        "turn/start" => {
                            assert_eq!(req["params"]["threadId"], id);
                            assert_eq!(req["params"]["input"][0]["text"], "fixture-only new input");
                            assert_ne!(s.0, "working");
                            s.2 += 1;
                            s.0 = "working".into();
                            s.1 = if s.4 {
                                "concurrent-human-turn".into()
                            } else {
                                format!("started-{}", s.2)
                            };
                            if s.3 {
                                break;
                            }
                            json!({"turn":{"id":s.1,"status":"inProgress"}})
                        }
                        other => panic!("forbidden operation {other}"),
                    }
                };
                let response = if result["rejected"] == true {
                    json!({"id":req["id"],"error":{"code":-32000,"message":"conflict"}})
                } else {
                    json!({"id":req["id"],"result":result})
                };
                if socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    });
    let target = || call(&root, &["modes", "target", "--session", id], None).1["result"].clone();
    let initial = target();
    assert_eq!(initial["actions"]["submit"]["available"], true);
    assert_eq!(initial["state"], "idle");
    assert_eq!(state.lock().unwrap().2, 0); // status never sends input
    let now = sessionsation::daemon::now();
    let mut req = json!({"request_id":"22222222-2222-4222-8222-222222222222","session_id":id,"mode":"submit","text":"fixture-only new input","expected_generation":initial["generation"],"expected_previous_turn_id":"previous","capability_id":initial["submit_capability_id"],"created_at":now,"expires_at":now+300});
    let (ok, accepted) = call(&root, &["modes", "request"], Some(&req));
    assert!(ok, "{accepted}");
    assert!(accepted["result"]["effective_action"].is_null());
    assert_eq!(accepted["result"]["native_operation"], "turn/start");
    assert_eq!(accepted["result"]["facts"]["input_consumed"], "unknown");
    assert_eq!(call(&root, &["modes", "request"], Some(&req)).1, accepted);
    assert_eq!(
        call(
            &root,
            &[
                "modes",
                "status",
                "--request",
                req["request_id"].as_str().unwrap()
            ],
            None
        )
        .1,
        accepted
    );
    assert_eq!(state.lock().unwrap().2, 1);
    let mut conflict = req.clone();
    conflict["resume"] = json!(true);
    assert_eq!(
        call(&root, &["modes", "request"], Some(&conflict)).1["error"]["code"],
        "idempotency_conflict"
    );
    req["request_id"] = json!("33333333-3333-4333-8333-333333333333");
    req["expected_previous_turn_id"] = json!("started-1");
    for terminal in ["interrupted", "errored"] {
        state.lock().unwrap().0 = terminal.into();
        let observed = target();
        assert_eq!(observed["state"], terminal);
        assert_eq!(observed["actions"]["submit"]["available"], false);
        assert_eq!(observed["actions"]["resume"]["available"], true);
        assert_eq!(
            call(&root, &["modes", "request"], Some(&req)).1["error"]["code"],
            "explicit_resume_required"
        );
    }
    // A recorded manual stop is a revisioned intent, never a recipient permit.
    std::fs::create_dir_all(root.join("conversations")).unwrap();
    let stop = "66666666-6666-4666-8666-666666666666";
    std::fs::write(
        root.join(format!("conversations/{id}.json")),
        json!({"owner":home,"stop_intent":stop}).to_string(),
    )
    .unwrap();
    assert_eq!(target()["stop_intent_id"], stop);
    req["resume"] = json!(true);
    assert_eq!(
        call(&root, &["modes", "request"], Some(&req)).1["error"]["code"],
        "stop_intent_changed"
    );
    req["expected_stop_id"] = json!(stop);
    let (ok, resumed) = call(&root, &["modes", "request"], Some(&req));
    assert!(ok, "{resumed}");
    assert_eq!(resumed["result"]["resume"], true);
    assert!(target()["stop_intent_id"].is_null());
    assert!(resumed["result"]["effective_action"].is_null());
    assert_eq!(state.lock().unwrap().2, 2);
    req["request_id"] = json!("44444444-4444-4444-8444-444444444444");
    req["resume"] = json!(false);
    req["expected_stop_id"] = Value::Null;
    req["expected_previous_turn_id"] = json!("started-2");
    {
        let mut s = state.lock().unwrap();
        s.0 = "idle".into();
        s.4 = true;
    }
    let (_, race) = call(&root, &["modes", "request"], Some(&req));
    assert_eq!(race["result"]["state"], "native_accepted");
    assert_eq!(
        race["result"]["acknowledged_turn_id"],
        "concurrent-human-turn"
    );
    assert!(race["result"]["effective_action"].is_null());
    assert_eq!(state.lock().unwrap().2, 3);
    assert_eq!(call(&root, &["modes", "request"], Some(&req)).1, race);
    req["request_id"] = json!("55555555-5555-4555-8555-555555555555");
    req["expected_previous_turn_id"] = json!("concurrent-human-turn");
    {
        let mut s = state.lock().unwrap();
        s.0 = "idle".into();
        s.3 = true;
        s.4 = false;
    }
    let (_, unknown) = call(&root, &["modes", "request"], Some(&req));
    assert_eq!(unknown["result"]["state"], "delivery_unknown");
    assert_eq!(state.lock().unwrap().2, 4);
    assert_eq!(call(&root, &["modes", "request"], Some(&req)).1, unknown);
    assert_eq!(state.lock().unwrap().2, 4);
    task.abort();
    assert_eq!(target()["state"], "disconnected");
}

#[test]
fn saved_owner_inventory_and_explicit_stop_are_not_a_recipient_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("conversations")).unwrap();
    for i in 1..=3 {
        let id = format!("00000000-0000-4000-8000-{i:012}");
        std::fs::write(
            root.join(format!("conversations/{id}.json")),
            json!({"owner":root.join("fixture-home"),"name":"同じ名前","deleted":i==3}).to_string(),
        )
        .unwrap();
    }
    let (ok, result) = call(root, &["modes", "sessions"], None);
    assert!(ok);
    let sessions = result["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    for session in sessions {
        assert_eq!(session["state"], "disconnected");
        assert!(session["generation"].is_null());
        assert_eq!(session["actions"]["resume"]["available"], false);
        assert!(session["observed_at"].is_number());
    }
    assert_ne!(sessions[0]["session_id"], sessions[1]["session_id"]);
    assert!(!result.to_string().contains("fixture-home"));
}
