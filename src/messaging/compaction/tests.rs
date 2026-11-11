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
            match method.as_str() {
                "thread/read" => Some(
                    json!({"thread":{"id":id,"status":{"type":if s.busy {"active"} else {"idle"}}}}),
                ),
                "thread/turns/list" => {
                    let mut data = s.turns.clone();
                    data.reverse();
                    data.truncate(r["params"]["limit"].as_u64().unwrap_or(50) as usize);
                    Some(json!({"data":data,"nextCursor":null}))
                }
                "thread/queue/list" => Some(json!({"data":s.queue.iter().map(|q|
                    json!({"id":format!("q-{q}"),"clientUserMessageId":q})).collect::<Vec<_>>(),"nextCursor":null})),
                "thread/queue/add" => {
                    let client = r["params"]["clientUserMessageId"].as_str().unwrap().to_owned();
                    // The session is idle: the add is consumed at once as a finished turn.
                    s.turns.push(json!({"id":format!("turn-{client}"),"status":"completed","itemsView":"summary",
                        "items":[{"type":"userMessage","id":"x","clientId":client}]}));
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
                mode: PublicMode::Queue,
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
