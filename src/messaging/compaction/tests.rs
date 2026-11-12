use super::*;
use crate::messaging::public::{PublicMode, PublicRequest, send_public};
use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_connections};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Fake {
    /// A turn is running (thread status `active`).
    busy: bool,
    /// Entries of the native queue.
    queue: Vec<String>,
    /// Remaining `thread/compact/start` calls answered with a JSON-RPC error.
    reject: u32,
    /// Answer `thread/compact/start` by dropping the connection (an unknown outcome).
    drop_start: bool,
    /// Status of the turn a start records (`inProgress` until a test completes it).
    compaction_status: Option<&'static str>,
    /// A queue add leaves a running turn (thread `active`) instead of a finished one.
    add_busy: bool,
    /// Thread reads fail (the connection drops): a transient provider error.
    fail_reads: bool,
    /// The summary view also omits user and agent messages.
    hide_summary_items: bool,
    turns: Vec<Value>,
    /// Every method received, in order, with the client id of an add.
    log: Vec<String>,
}
impl Fake {
    fn count(&self, method: &str) -> usize {
        self.log.iter().filter(|m| *m == method).count()
    }
    fn position(&self, method: &str) -> usize {
        self.log.iter().position(|m| m == method).unwrap()
    }
}

struct World {
    _dir: tempfile::TempDir,
    paths: Paths,
    conversation: String,
    session: crate::catalog::Session,
    input: Enqueue,
    fake: Arc<Mutex<Fake>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for World {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn world() -> World {
    let (dir, paths, input, _) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let fake = Arc::new(Mutex::new(Fake {
        compaction_status: Some("inProgress"),
        turns: vec![
            json!({"id":"turn-0","status":"completed","itemsView":"summary",
            "items":[{"type":"userMessage","id":"i0"}]}),
        ],
        ..Default::default()
    }));
    let shared = fake.clone();
    let id = a.id.clone();
    let server = scripted_connections(
        &a.endpoint(),
        move |r| {
            let mut s = shared.lock().unwrap();
            let method = r["method"].as_str().unwrap().to_owned();
            s.log.push(method.clone());
            if r["params"]["itemsView"] == "full" {
                s.log.push("full-turn-read".into());
            }
            match method.as_str() {
                "thread/read" if s.fail_reads => None,
                "thread/read" => Some(
                    json!({"thread":{"id":id,"status":{"type":if s.busy {"active"} else {"idle"}}}}),
                ),
                "thread/turns/list" => {
                    // Real app-server 0.163-0.166 shape (checked against 0.166.0 on a copy of the
                    // incident thread): newest first, an opaque `nextCursor` after the last turn;
                    // the summary view keeps only user and agent messages (a compaction turn is
                    // `items: []`), the full view lists `contextCompaction` once it completed.
                    let mut data = s.turns.clone();
                    data.reverse();
                    if let Some(cursor) = r["params"]["cursor"].as_str() {
                        let anchor: Value = serde_json::from_str(cursor).unwrap();
                        let at = data.iter().position(|t| t["id"] == anchor["turnId"]).unwrap();
                        let skip = if anchor["includeAnchor"] == true { at } else { at + 1 };
                        data.drain(..skip);
                    }
                    let limit = r["params"]["limit"].as_u64().unwrap_or(50) as usize;
                    let more = data.len() > limit;
                    data.truncate(limit);
                    let full = r["params"]["itemsView"] == "full";
                    for turn in data.iter_mut() {
                        let running = turn["status"] == "inProgress";
                        let items: Vec<Value> = turn["items"].as_array().cloned().unwrap_or_default().into_iter()
                            .filter(|i| if full { !(running && i["type"] == "contextCompaction") }
                                else { !s.hide_summary_items && matches!(i["type"].as_str(), Some("userMessage" | "agentMessage")) })
                            .collect();
                        turn["items"] = json!(items);
                        turn["itemsView"] = json!(if full { "full" } else { "summary" });
                    }
                    let next = more.then(|| json!({"turnId":data.last().unwrap()["id"],"includeAnchor":false}).to_string());
                    Some(json!({"data":data,"nextCursor":next}))
                }
                // "thread/items/list is not supported yet" for legacy-history threads.
                "thread/items/list" => Some(json!({"__error":-32601})),
                "thread/queue/list" => Some(json!({"data":s.queue.iter().map(|q|
                    json!({"id":format!("q-{q}"),"clientUserMessageId":q})).collect::<Vec<_>>(),"nextCursor":null})),
                "thread/queue/add" => {
                    let client = r["params"]["clientUserMessageId"].as_str().unwrap().to_owned();
                    // The session is idle: the add is consumed at once as a finished turn.
                    let add_status = if s.add_busy { "inProgress" } else { "completed" };
                    s.turns.push(json!({"id":format!("turn-{client}"),"status":add_status,"itemsView":"summary",
                        "items":[{"type":"userMessage","id":"x","clientId":client}]}));
                    s.busy = s.add_busy;
                    Some(json!({"queuedSubmission":{"id":format!("q-{client}"),"clientUserMessageId":client}}))
                }
                "thread/compact/start" => {
                    if s.drop_start {
                        return None;
                    }
                    if s.reject > 0 {
                        s.reject -= 1;
                        return Some(json!({"__error":true}));
                    }
                    let status = s.compaction_status.unwrap();
                    s.turns.push(json!({"id":"turn-compact","status":status,"itemsView":"summary",
                        "items":[{"type":"contextCompaction","id":"c1"}]}));
                    s.busy = status == "inProgress";
                    Some(json!({}))
                }
                other => panic!("unexpected {other}"),
            }
        },
        400,
    )
    .await;
    let instance: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture",
        "pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,
        "binary":"fixture","resume_options":[],"account":"selected"}))
    .unwrap();
    let session = crate::catalog::reconcile(&paths, &[instance])
        .unwrap()
        .remove(0);
    World {
        _dir: dir,
        paths,
        conversation: a.id.clone(),
        session,
        input: Enqueue {
            expected_generation: a.generation(),
            expires_at: crate::daemon::now() + 300,
            ..input
        },
        fake,
        server,
    }
}

impl World {
    fn compact(&self) -> Value {
        request(
            &self.paths,
            Request {
                session: &self.session.id,
                expected_generation: &self.session.generation(),
                expires_at: None,
                operation: None,
            },
        )
        .unwrap()
    }
    fn record(&self, id: &str) -> Record {
        read(&self.paths, id).unwrap()
    }
    fn phase(&self, id: &str) -> String {
        self.record(id).phase
    }
    /// A queue-mode send from another agent.
    async fn send(&self, id: &str) -> Value {
        self.send_mode(id, PublicMode::Queue).await
    }
    async fn send_mode(&self, id: &str, mode: PublicMode) -> Value {
        send_public(
            &self.paths,
            PublicRequest {
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                steering: None,
                recovery: None,
                api_version: 1,
                message_id: id.into(),
                session_id: self.session.id.clone(),
                expected_generation: self.session.generation(),
                mode,
                text: format!("message {id}"),
                expires_at: crate::daemon::now() + 300,
            },
        )
        .await
        .unwrap()
    }
    fn receipt_state(&self, id: &str) -> (String, Option<String>) {
        let ledger = Ledger::open(&self.paths).unwrap();
        let r = ledger.get(id).unwrap().unwrap();
        (r.state, r.reason)
    }
    fn complete_compaction(&self, status: &'static str) {
        let mut s = self.fake.lock().unwrap();
        for turn in s.turns.iter_mut() {
            if turn["id"] == "turn-compact" {
                turn["status"] = json!(status);
            }
        }
        s.busy = false;
    }
}
const M1: &str = "11111111-1111-4111-8111-111111111111";
const M2: &str = "22222222-2222-4222-8222-222222222222";

#[tokio::test]
async fn waits_for_the_idle_point_then_starts_and_completes() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let op = w.compact();
    let id = op["id"].as_str().unwrap().to_owned();
    assert_eq!(
        (op["phase"].as_str(), op["existing"].as_bool()),
        (Some("waiting"), Some(false))
    );
    assert_eq!(op["hold"]["active"], true);
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("waiting", Some("turn_active"))
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 0);
    // The turn ends: the next pass starts it exactly once.
    w.fake.lock().unwrap().busy = false;
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    assert_eq!(w.record(&id).attempts, 1);
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running", "still compacting");
    w.complete_compaction("completed");
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.compaction_turn_id.as_deref()),
        ("complete", Some("turn-compact"))
    );
    assert!(r.finished_at.is_some());
    tick(&w.paths).await;
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 1);
    // Visible in operations and status.
    assert!(
        operations(&w.paths)
            .unwrap()
            .iter()
            .any(|o| o["id"] == id.as_str())
    );
    let status = status(&w.paths, Some(&id), None).await.unwrap();
    assert_eq!(status["phase"], "complete");
    assert_eq!(status["held_messages"], json!([]));
}

#[tokio::test]
async fn one_pending_compaction_per_session_and_a_repeat_returns_it() {
    let w = world().await;
    let first = w.compact();
    let again = w.compact();
    assert_eq!(again["id"], first["id"]);
    assert_eq!(again["existing"], true);
    // A different caller operation id for the same session still reports the pending one.
    let other = request(
        &w.paths,
        Request {
            session: &w.session.id,
            expected_generation: &w.session.generation(),
            expires_at: Some(crate::daemon::now() + 60),
            operation: Some("33333333-3333-4333-8333-333333333333"),
        },
    )
    .unwrap();
    assert_eq!(other["id"], first["id"]);
    assert_eq!(list(&w.paths).unwrap().len(), 1);
    // The caller's own id is honoured when nothing is pending.
    w.fake.lock().unwrap().compaction_status = Some("completed");
    tick(&w.paths).await;
    tick(&w.paths).await;
    assert_eq!(w.phase(first["id"].as_str().unwrap()), "complete");
    let next = request(
        &w.paths,
        Request {
            session: &w.session.id,
            expected_generation: &w.session.generation(),
            expires_at: None,
            operation: Some("33333333-3333-4333-8333-333333333333"),
        },
    )
    .unwrap();
    assert_eq!(next["id"], "33333333-3333-4333-8333-333333333333");
    assert_eq!(next["existing"], false);
}

#[tokio::test]
async fn input_is_held_during_the_compaction_and_delivered_after_it_once_in_order() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    // Two senders while the turn is still running: nothing reaches the native queue.
    for message in [M1, M2] {
        let receipt = w.send(message).await;
        assert_eq!(receipt["state"], "pending_local", "{receipt}");
        assert_eq!(receipt["reason"], "compaction_hold");
        assert_eq!(receipt["facts"]["accepted_native"], false);
    }
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 0);
    let held = status(&w.paths, Some(&id), None).await.unwrap();
    assert_eq!(held["held_messages"], json!([M1, M2]));
    // Idle: the compaction starts, input stays held while it runs.
    w.fake.lock().unwrap().busy = false;
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    let third = w.send("44444444-4444-4444-8444-444444444444").await;
    assert_eq!(third["reason"], "compaction_hold");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 0);
    // Completed: all three are delivered, once each, after the compaction start.
    w.complete_compaction("completed");
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "complete");
    assert!(w.record(&id).hold_released);
    {
        let fake = w.fake.lock().unwrap();
        assert_eq!(fake.count("thread/queue/add"), 3, "{:?}", fake.log);
        assert!(fake.position("thread/compact/start") < fake.position("thread/queue/add"));
        let order: Vec<_> = fake
            .turns
            .iter()
            .filter_map(|t| t["items"][0]["clientId"].as_str())
            .collect();
        assert_eq!(order, [M1, M2, "44444444-4444-4444-8444-444444444444"]);
    }
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
    tick(&w.paths).await;
    assert_eq!(
        w.fake.lock().unwrap().count("thread/queue/add"),
        3,
        "no duplicate delivery"
    );
    // With nothing pending, a new send is delivered at once.
    let fresh = w.send("55555555-5555-4555-8555-555555555555").await;
    assert_ne!(fresh["reason"], "compaction_hold");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 4);
}

#[tokio::test]
async fn a_failed_compaction_releases_the_hold() {
    let w = world().await;
    w.fake.lock().unwrap().compaction_status = Some("inProgress");
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    w.complete_compaction("failed");
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("compaction_failed"))
    );
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 1);
}

#[tokio::test]
async fn already_native_queued_input_runs_first_and_the_compaction_waits_for_it() {
    let w = world().await;
    w.fake.lock().unwrap().queue = vec!["earlier".into()];
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("waiting", Some("native_queue_not_empty"))
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 0);
    w.fake.lock().unwrap().queue.clear();
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
}

#[tokio::test]
async fn cancel_releases_held_input_and_is_refused_once_started() {
    let w = world().await;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    w.fake.lock().unwrap().busy = true;
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    w.fake.lock().unwrap().busy = false;
    let cancelled = cancel(&w.paths, &id).await.unwrap();
    assert_eq!(cancelled["phase"], "cancelled");
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 0);
    // Idempotent; the next request starts a fresh operation.
    assert_eq!(cancel(&w.paths, &id).await.unwrap()["phase"], "cancelled");
    let next = w.compact();
    assert_ne!(next["id"], id.as_str());
    tick(&w.paths).await;
    let next_id = next["id"].as_str().unwrap();
    assert_eq!(w.phase(next_id), "running");
    let error = cancel(&w.paths, next_id).await.unwrap_err().to_string();
    assert!(error.starts_with("COMPACTION_ALREADY_STARTED"), "{error}");
    w.complete_compaction("completed");
    tick(&w.paths).await;
    let error = cancel(&w.paths, next_id).await.unwrap_err().to_string();
    assert!(error.starts_with("COMPACTION_ALREADY_FINISHED"), "{error}");
}

#[tokio::test]
async fn an_expired_compaction_never_starts_and_releases_held_input() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    update(&w.paths, &id, |r| {
        r.expires_at = crate::daemon::now() - 1;
        true
    })
    .unwrap();
    w.fake.lock().unwrap().busy = false;
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("expired", Some("expired"))
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 0);
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
    // The expiry is bounded like a send.
    for bad in [
        Some(crate::daemon::now() - 5),
        Some(crate::daemon::now() + MAX_EXPIRY_SECONDS + 60),
    ] {
        let error = request(
            &w.paths,
            Request {
                session: &w.session.id,
                expected_generation: &w.session.generation(),
                expires_at: bad,
                operation: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("INVALID_EXPIRY"), "{error}");
    }
}

#[tokio::test]
async fn a_refusing_app_server_is_retried_then_fails_and_releases_input() {
    let w = world().await;
    w.fake.lock().unwrap().reject = 10;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref(), r.attempts),
        ("waiting", Some("provider_rejected"), 1)
    );
    assert_eq!(
        w.receipt_state(M1).0,
        "pending_local",
        "still held between attempts"
    );
    tick(&w.paths).await;
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref(), r.attempts),
        ("failed", Some("provider_rejected"), 3)
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 3);
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
}

#[tokio::test]
async fn an_unanswered_start_is_never_repeated_on_a_guess() {
    let w = world().await;
    w.fake.lock().unwrap().drop_start = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "starting");
    w.fake.lock().unwrap().drop_start = false;
    tick(&w.paths).await;
    tick(&w.paths).await;
    assert_eq!(
        w.phase(&id),
        "starting",
        "no compaction turn seen yet, not started again"
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 1);
    // The turn shows up: it was started after all.
    w.fake
        .lock()
        .unwrap()
        .turns
        .push(json!({"id":"turn-compact","status":"completed",
        "itemsView":"summary","items":[{"type":"contextCompaction","id":"c1"}]}));
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "complete");
}

#[tokio::test]
async fn a_restarted_session_fails_the_operation_and_releases_input() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    update(&w.paths, &id, |r| {
        r.runtime_generation = "0".repeat(64);
        true
    })
    .unwrap();
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("generation_changed"))
    );
    // The live attachment is unchanged, so the held message is still delivered, once.
    assert_ne!(w.receipt_state(M1).0, "pending_local");
}

#[tokio::test]
async fn sessions_of_other_providers_are_refused_with_a_clear_code() {
    let w = world().await;
    // Re-label the catalog entry as a Claude session.
    let file = w
        .paths
        .root
        .join(crate::layout::SESSION_CATALOG)
        .join("catalog.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    v["sessions"][&w.session.id]["provider"] = json!("claude");
    fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
    let generation = crate::catalog::find(&w.paths, &w.session.id)
        .unwrap()
        .generation();
    let error = request(
        &w.paths,
        Request {
            session: &w.session.id,
            expected_generation: &generation,
            expires_at: None,
            operation: None,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("COMPACT_UNSUPPORTED"), "{error}");
    assert!(error.contains("compact themselves"), "{error}");
    assert!(list(&w.paths).unwrap().is_empty());
    assert!(!hold_active(&w.paths, &w.conversation));
}

/// Start, complete and leave the operation just before its release.
async fn run_to_complete(w: &World, id: &str) {
    w.fake.lock().unwrap().compaction_status = Some("completed");
    tick(&w.paths).await;
    assert_eq!(w.phase(id), "running");
}

#[tokio::test]
async fn an_unavailable_ledger_keeps_the_hold_and_a_new_send_cannot_overtake() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    w.fake.lock().unwrap().busy = false;
    run_to_complete(&w, &id).await;
    // The release cannot read the ledger: the operation finishes but the hold stays.
    let blocker = Ledger::open(&w.paths).unwrap();
    tick(&w.paths).await;
    drop(blocker);
    let r = w.record(&id);
    assert_eq!(r.phase, "complete");
    assert!(
        !r.hold_released,
        "nothing was released, so it is not marked released"
    );
    assert!(hold_active(&w.paths, &w.conversation));
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 0);
    // A send arriving now is held behind M1, not delivered ahead of it.
    assert_eq!(w.send(M2).await["reason"], "compaction_hold");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 0);
    tick(&w.paths).await;
    assert!(w.record(&id).hold_released);
    assert!(!hold_active(&w.paths, &w.conversation));
    let fake = w.fake.lock().unwrap();
    let order: Vec<_> = fake
        .turns
        .iter()
        .filter_map(|t| t["items"][0]["clientId"].as_str())
        .collect();
    assert_eq!(order, [M1, M2]);
}

#[tokio::test]
async fn a_delivery_that_fails_transiently_is_retried_and_blocks_the_hold_release() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    for m in [M1, M2] {
        assert_eq!(w.send(m).await["reason"], "compaction_hold");
    }
    w.fake.lock().unwrap().busy = false;
    run_to_complete(&w, &id).await;
    // The owner's native-send switch is off: delivery is refused, the receipts stay local.
    let marker = w.paths.root.join("messaging/native-send.disabled");
    fs::write(&marker, "").unwrap();
    tick(&w.paths).await;
    assert!(!w.record(&id).hold_released);
    assert_eq!(
        w.receipt_state(M1),
        (
            "pending_local".to_string(),
            Some("compaction_release_retry".to_string())
        )
    );
    assert_eq!(w.receipt_state(M2).0, "pending_local");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 0);
    // Repaired: the next tick re-queries, delivers both in order and only then releases.
    fs::remove_file(&marker).unwrap();
    tick(&w.paths).await;
    assert!(w.record(&id).hold_released);
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
    assert_eq!(w.receipt_state(M2).0, "native_accepted");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 2);
}

#[tokio::test]
async fn held_submit_messages_are_all_released_with_queue_semantics() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    for m in [M1, M2] {
        assert_eq!(
            w.send_mode(m, PublicMode::Submit).await["reason"],
            "compaction_hold"
        );
    }
    {
        // Every add starts a turn that is still running when the next one is delivered.
        let mut fake = w.fake.lock().unwrap();
        fake.busy = false;
        fake.add_busy = true;
    }
    run_to_complete(&w, &id).await;
    tick(&w.paths).await;
    assert!(w.record(&id).hold_released);
    for m in [M1, M2] {
        assert_eq!(w.receipt_state(m).0, "native_accepted", "{m}");
    }
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 2);
    // The receipt says how it was delivered.
    let receipt = w.send_mode(M2, PublicMode::Submit).await;
    assert_eq!(receipt["mode"], "submit");
    assert_eq!(receipt["hold_release"]["delivered_as"], "queue");
}

#[tokio::test]
async fn an_unobserved_start_fails_without_being_issued_again_and_releases_the_hold() {
    let w = world().await;
    w.fake.lock().unwrap().drop_start = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "starting");
    w.fake.lock().unwrap().drop_start = false;
    // Long past the window, idle, no compaction turn: failed, never started a second time.
    let mut r = w.record(&id);
    r.updated_at = crate::daemon::now() - 3 * OBSERVE_GRACE_SECONDS;
    save(&w.paths, &r).unwrap();
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("compaction_not_observed"))
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 1);
    assert!(r.hold_released);
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
}

#[tokio::test]
async fn only_a_positive_compaction_item_counts_as_the_compaction_turn() {
    let w = world().await;
    {
        let mut fake = w.fake.lock().unwrap();
        fake.drop_start = true;
        // The summary omits item lists: each new turn is read item by item.
        fake.hide_summary_items = true;
    }
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "starting");
    // A human-typed turn and a turn with only an agent message are not the compaction.
    {
        let mut fake = w.fake.lock().unwrap();
        fake.turns.push(
            json!({"id":"human","status":"completed","items":[{"type":"userMessage","id":"u"}]}),
        );
        fake.turns.push(
            json!({"id":"other","status":"completed","items":[{"type":"agentMessage","id":"a"}]}),
        );
    }
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "starting");
    assert!(
        w.fake.lock().unwrap().count("full-turn-read") >= 2,
        "each new turn without a visible user message was read in full"
    );
    w.fake.lock().unwrap().turns.push(
        json!({"id":"real","status":"completed","items":[{"type":"contextCompaction","id":"c"}]}),
    );
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.compaction_turn_id.as_deref()),
        ("complete", Some("real"))
    );
}

#[tokio::test]
async fn a_waiting_operation_past_its_expiry_no_longer_holds_input() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert!(hold_active(&w.paths, &w.conversation));
    // The daemon is down: nobody marked it expired.
    update(&w.paths, &id, |r| {
        r.expires_at = crate::daemon::now() - 1;
        true
    })
    .unwrap();
    assert_eq!(w.phase(&id), "waiting");
    assert!(!hold_active(&w.paths, &w.conversation));
    let receipt = w.send(M1).await;
    assert_ne!(receipt["reason"], "compaction_hold", "{receipt}");
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
}

#[tokio::test]
async fn the_legacy_enqueue_path_is_not_held() {
    let w = world().await;
    w.fake.lock().unwrap().busy = true;
    w.compact();
    let mut ledger = Ledger::open(&w.paths).unwrap();
    let mut receipt = ledger.accept(&w.input, crate::daemon::now()).unwrap();
    deliver(&w.paths, &mut ledger, &mut receipt, Some(&w.input))
        .await
        .unwrap();
    assert_eq!(receipt.state, "native_accepted");
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 1);
}

/// Move a started operation's start back by `seconds` (as if that much time had passed).
fn backdate(w: &World, id: &str, seconds: i64) {
    let mut r = w.record(id);
    let now = crate::daemon::now();
    r.started_at = r.started_at.map(|_| now - seconds);
    r.updated_at = now - seconds;
    save(&w.paths, &r).unwrap();
}

/// Regression for operation 55e40f43 (contract 1.47.1): the compaction is a new turn whose summary
/// lists no items at all, `thread/items/list` is refused with -32601, and a human turn may already
/// follow it. 1.47.0 returned silently on the refused item read every pass and never finished.
#[tokio::test]
async fn the_real_compaction_turn_shape_is_observed_and_completes() {
    let w = world().await;
    {
        let mut fake = w.fake.lock().unwrap();
        for n in 1..5 {
            fake.turns
                .push(json!({"id":format!("turn-{n}"),"status":"completed",
                "items":[{"type":"userMessage","id":"u"},{"type":"agentMessage","id":"a"}]}));
        }
    }
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!((r.phase.as_str(), r.baseline.len()), ("running", 5));
    // While it runs the full view does not show the item yet: still running, not failed.
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    w.complete_compaction("completed");
    // The pane is used right after: a newer human turn now precedes the compaction turn.
    w.fake
        .lock()
        .unwrap()
        .turns
        .push(json!({"id":"turn-human","status":"completed",
        "items":[{"type":"userMessage","id":"h"},{"type":"agentMessage","id":"ha"}]}));
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.compaction_turn_id.as_deref()),
        ("complete", Some("turn-compact")),
    );
    tick(&w.paths).await;
    let fake = w.fake.lock().unwrap();
    assert_eq!(fake.count("thread/items/list"), 0, "{:?}", fake.log);
    assert!(fake.count("full-turn-read") >= 2);
    drop(fake);
    assert!(w.record(&id).hold_released);
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
}

#[tokio::test]
async fn a_compaction_that_cannot_be_read_fails_after_the_unobserved_limit_and_releases() {
    let w = world().await;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    assert_eq!(w.send(M1).await["reason"], "compaction_hold");
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    w.fake.lock().unwrap().fail_reads = true;
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running", "a transient read error is retried");
    backdate(&w, &id, UNOBSERVED_LIMIT_SECONDS + 5);
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("compaction_not_observed"))
    );
    w.fake.lock().unwrap().fail_reads = false;
    w.complete_compaction("completed");
    tick(&w.paths).await;
    assert!(w.record(&id).hold_released);
    assert_eq!(w.receipt_state(M1).0, "native_accepted");
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 1);
}

#[tokio::test]
async fn a_non_idle_thread_without_a_compaction_turn_fails_after_the_unobserved_limit() {
    let w = world().await;
    w.fake.lock().unwrap().drop_start = true;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "starting");
    {
        // Background work keeps the thread active; no compaction turn ever shows.
        let mut fake = w.fake.lock().unwrap();
        fake.drop_start = false;
        fake.busy = true;
    }
    backdate(&w, &id, 3 * OBSERVE_GRACE_SECONDS);
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "starting", "within the unobserved limit");
    backdate(&w, &id, UNOBSERVED_LIMIT_SECONDS + 5);
    tick(&w.paths).await;
    let r = w.record(&id);
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("compaction_not_observed"))
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/compact/start"), 1);
}

impl World {
    /// The steer a busy-target sender issues after `api steer-target` named `turn`.
    async fn steer(&self, id: &str, turn: &str) -> Value {
        send_public(
            &self.paths,
            PublicRequest {
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                steering: Some(crate::messaging::public::SteeringIntent {
                    version: 1,
                    expected_turn_id: turn.into(),
                    expected_runtime_generation: self.input.expected_generation.clone(),
                }),
                recovery: None,
                api_version: 1,
                message_id: id.into(),
                session_id: self.session.id.clone(),
                expected_generation: self.session.generation(),
                mode: PublicMode::Steer,
                text: format!("message {id}"),
                expires_at: crate::daemon::now() + 300,
            },
        )
        .await
        .unwrap()
    }
    async fn steer_target(&self) -> Value {
        crate::messaging::public::steering_target(
            &self.paths,
            &self.session.id,
            &self.session.generation(),
        )
        .await
        .unwrap()
    }
}

/// Incident 2026-11-12 (contract 1.48.1): an agent queued its own compaction and a resume note
/// for after it. The note was steered into the running turn (delivered before the compaction,
/// which then summarized it away) and a later message was steered into the compaction turn
/// itself (refused natively); after the compaction the agent sat idle with nothing to run.
/// While a compaction is queued or running, steering is refused before any native call with
/// `target_compacting`, `steer-target` says so, and a submit is held and delivered as a queued
/// turn after the compaction.
#[tokio::test]
async fn no_steering_while_a_compaction_is_pending_and_the_submit_lands_after_it() {
    const STEER_BEFORE: &str = "66666666-6666-4666-8666-666666666666";
    const STEER_INTO: &str = "77777777-7777-4777-8777-777777777777";
    const SUBMIT: &str = "88888888-8888-4888-8888-888888888888";
    let w = world().await;
    {
        // The agent's own turn is running: the real summary shape, a user message first.
        let mut s = w.fake.lock().unwrap();
        s.busy = true;
        s.turns.push(
            json!({"id":"turn-busy","status":"inProgress","itemsView":"summary",
            "items":[{"type":"userMessage","id":"u1"}]}),
        );
    }
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    let target = w.steer_target().await;
    assert_eq!(
        (target["available"].as_bool(), target["reason"].as_str()),
        (Some(false), Some("target_compacting")),
        "{target}"
    );
    assert!(target["active_turn_id"].is_null());
    let before = w.steer(STEER_BEFORE, "turn-busy").await;
    assert_eq!(
        (before["state"].as_str(), before["reason"].as_str()),
        (Some("rejected"), Some("target_compacting")),
        "{before}"
    );
    assert_eq!(before["facts"]["accepted_native"], false);
    // A submit with if_busy queue (what a sender falls back to) is held, not steered.
    let held = send_public(
        &w.paths,
        PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: Some("queue".into()),
            steering: None,
            recovery: None,
            api_version: 1,
            message_id: SUBMIT.into(),
            session_id: w.session.id.clone(),
            expected_generation: w.session.generation(),
            mode: PublicMode::Submit,
            text: "resume note".into(),
            expires_at: crate::daemon::now() + 300,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (held["state"].as_str(), held["reason"].as_str()),
        (Some("pending_local"), Some("compaction_hold")),
        "{held}"
    );
    // The turn ends; the compaction turn runs (summary `items: []`, as real app-servers list it).
    {
        let mut s = w.fake.lock().unwrap();
        s.busy = false;
        for turn in s.turns.iter_mut() {
            if turn["id"] == "turn-busy" {
                turn["status"] = json!("completed");
            }
        }
    }
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    let target = w.steer_target().await;
    assert_eq!(target["reason"], "target_compacting", "{target}");
    let into = w.steer(STEER_INTO, "turn-compact").await;
    assert_eq!(
        (into["state"].as_str(), into["reason"].as_str()),
        (Some("rejected"), Some("target_compacting")),
        "{into}"
    );
    assert_eq!(w.fake.lock().unwrap().count("thread/queue/add"), 0);
    // Completed: the held submit starts its own turn after the compaction.
    w.complete_compaction("completed");
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "complete");
    assert!(w.record(&id).hold_released);
    let fake = w.fake.lock().unwrap();
    assert_eq!(fake.count("turn/steer"), 0, "{:?}", fake.log);
    assert_eq!(fake.count("thread/queue/add"), 1, "{:?}", fake.log);
    assert!(fake.position("thread/compact/start") < fake.position("thread/queue/add"));
    assert_eq!(fake.turns.last().unwrap()["items"][0]["clientId"], SUBMIT);
    drop(fake);
    assert_eq!(w.receipt_state(SUBMIT).0, "native_accepted");
    for steered in [STEER_BEFORE, STEER_INTO] {
        assert_eq!(
            w.receipt_state(steered),
            ("rejected".into(), Some("target_compacting".into()))
        );
    }
}

#[tokio::test]
async fn a_long_running_compaction_turn_is_not_cut_off_by_the_unobserved_limit() {
    let w = world().await;
    let id = w.compact()["id"].as_str().unwrap().to_owned();
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "running");
    backdate(&w, &id, UNOBSERVED_LIMIT_SECONDS + 60);
    tick(&w.paths).await;
    assert_eq!(
        w.phase(&id),
        "running",
        "the compaction turn is still running"
    );
    w.complete_compaction("completed");
    tick(&w.paths).await;
    assert_eq!(w.phase(&id), "complete");
}

#[test]
fn diagnostics_are_rate_limited_per_operation_and_reason() {
    let op = "33333333-3333-4333-8333-333333333333";
    assert!(log_due(op, "turn_read", 1000));
    assert!(!log_due(op, "turn_read", 1000 + LOG_EVERY_SECONDS - 1));
    assert!(log_due(op, "connect", 1001));
    assert!(log_due(op, "turn_read", 1000 + LOG_EVERY_SECONDS));
    prune(&[]);
    assert!(log_due(op, "turn_read", 1000 + LOG_EVERY_SECONDS + 1));
}
