use super::super::*;
use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_server};

#[test]
fn reading_a_pending_receipt_settles_it_when_expired_or_the_session_was_replaced() {
    let (_dir, paths, mut input, _) = fixture();
    let now = crate::daemon::now();
    input.expires_at = now + 600;
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    input.expected_generation = a.generation();
    let mut ledger = Ledger::open(&paths).unwrap();
    // Still valid and the same generation: stays pending (transient reasons keep waiting).
    let mut r = ledger.accept(&input, now).unwrap();
    r.reason = Some("migrating".into());
    settle_pending(&paths, &mut ledger, &mut r).unwrap();
    assert_eq!(
        (r.state.as_str(), r.reason.as_deref()),
        ("pending_local", Some("migrating"))
    );
    // The session behind it was replaced: terminal, nothing was sent.
    r.generation = "f".repeat(64);
    settle_pending(&paths, &mut ledger, &mut r).unwrap();
    assert_eq!(
        (r.state.as_str(), r.reason.as_deref()),
        ("rejected", Some("generation_mismatch"))
    );
    assert_eq!(
        ledger.get(&r.message_id).unwrap().unwrap().state,
        "rejected"
    );
    // Terminal receipts are left alone.
    settle_pending(&paths, &mut ledger, &mut r).unwrap();
    assert_eq!(r.state, "rejected");
    // Past its expiry a pending receipt is expired when read, never pending for ever.
    let mut late = input.clone();
    late.message_id = uuid::Uuid::new_v4().to_string();
    late.expires_at = now + 1;
    let mut r = ledger.accept(&late, now).unwrap();
    r.reason = Some("migrating".into());
    ledger
        .db
        .execute(
            "UPDATE receipts SET expiry=? WHERE id=?",
            params![now - 1, r.message_id],
        )
        .unwrap();
    settle_pending(&paths, &mut ledger, &mut r).unwrap();
    assert_eq!((r.state.as_str(), r.reason.as_deref()), ("expired", None));
}
fn binding(input: Enqueue) -> Binding {
    Binding {
        cancelled: false,
        released_hold: false,
        origin: None,
        origin_source: None,
        evidence_observed_at: None,
        plan: None,
        request: PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: None,
            steering: None,
            api_version: 1,
            message_id: input.message_id.clone(),
            session_id: uuid::Uuid::new_v4().to_string(),
            expected_generation: "a".repeat(64),
            recovery: None,
            mode: PublicMode::Queue,
            text: input.text.clone(),
            expires_at: input.expires_at,
        },
        native: input,
    }
}
#[test]
fn only_correlated_explicit_recovery_releases_its_original_stop() {
    let (_dir, paths, input, now) = fixture();
    let mut b = binding(input.clone());
    b.request.mode = PublicMode::Resume;
    b.request.recovery = Some(super::super::super::recovery::Intent {
        expected_state: "stopped".into(),
        expected_previous_turn_id: None,
        expected_stop_id: Some("original-stop".into()),
        expected_runtime_generation: None,
        launch: None,
    });
    let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    r.state = "native_accepted".into();
    let dir = paths.root.join(crate::layout::CONVERSATIONS);
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{}.json", r.session_id));
    let set = |stop: &str| {
        fs::write(
            &file,
            json!({"name":null,"owner":null,"stop_intent":stop}).to_string(),
        )
        .unwrap()
    };
    set("original-stop");
    reconcile_recovered_stop(&paths, &b, &r).unwrap();
    assert_eq!(
        external::messaging_stop_intent(&paths, &r.session_id)
            .unwrap()
            .as_deref(),
        Some("original-stop")
    );
    r.state = "execution_started".into();
    r.correlation = Some(observe::Correlation {
        turn_id: "turn-1".into(),
        user_item_id: "item-1".into(),
        completed_user_item_id: None,
        observed_generation: r.generation.clone(),
    });
    reconcile_recovered_stop(&paths, &b, &r).unwrap();
    assert!(
        external::messaging_stop_intent(&paths, &r.session_id)
            .unwrap()
            .is_none()
    );
    set("newer-stop");
    reconcile_recovered_stop(&paths, &b, &r).unwrap();
    assert_eq!(
        external::messaging_stop_intent(&paths, &r.session_id)
            .unwrap()
            .as_deref(),
        Some("newer-stop")
    );
    set("original-stop");
    b.cancelled = true;
    reconcile_recovered_stop(&paths, &b, &r).unwrap();
    assert_eq!(
        external::messaging_stop_intent(&paths, &r.session_id)
            .unwrap()
            .as_deref(),
        Some("original-stop")
    );
    b.cancelled = false;
    b.request.mode = PublicMode::Queue;
    reconcile_recovered_stop(&paths, &b, &r).unwrap();
    assert_eq!(
        external::messaging_stop_intent(&paths, &r.session_id)
            .unwrap()
            .as_deref(),
        Some("original-stop")
    );
}
fn save_binding(paths: &Paths, b: &Binding, r: &Receipt) {
    let mut ledger = Ledger::open(paths).unwrap();
    ledger.save(r).unwrap();
    ledger
        .db
        .execute(
            "INSERT INTO public_messages VALUES(?,?)",
            params![b.request.message_id, serde_json::to_string(b).unwrap()],
        )
        .unwrap();
}
#[tokio::test]
async fn passive_receipt_releases_writer_lease_and_cancellation_wins() {
    use crate::messaging::tests::{attachment_fixture, bind, scripted_server};
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    input.expected_generation = a.generation();
    let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    r.state = "native_accepted".into();
    r.native_queue_id = Some("queue-1".into());
    save_binding(&paths, &binding(input.clone()), &r);
    let root = paths.root.clone();
    let runtime = paths.runtime.clone();
    let message = input.message_id.clone();
    let server = scripted_server(&a.endpoint(), move |request| {
        if request["method"] == "thread/read" {
            return Some(json!({"thread":{"id":request["params"]["threadId"],"path":null}}));
        }
        assert_eq!(request["method"], "thread/turns/list");
        // Models another caller committing cancellation while the native
        // observation is outstanding. The root-wide lease must be free.
        let p = Paths {
            root: root.clone(),
            runtime: runtime.clone(),
        };
        let mut ledger = Ledger::open(&p).unwrap();
        let mut receipt = ledger.get(&message).unwrap().unwrap();
        receipt.state = "cancelled".into();
        ledger.save(&receipt).unwrap();
        Some(
            serde_json::from_str::<Value>(include_str!(
                "../../../../docs/messaging/v1.2.1/fixtures.json"
            ))
            .unwrap()["native_summary"]
                .clone(),
        )
    })
    .await;
    let out = receipt_public(&paths, &input.message_id, false)
        .await
        .unwrap();
    assert_eq!(out["state"], "cancelled");
    assert_eq!(out["facts"]["input_recorded"], false);
    assert_eq!(
        out["observation"]["error_code"],
        "receipt_changed_during_observation"
    );
    assert_eq!(
        Ledger::open(&paths)
            .unwrap()
            .get(&input.message_id)
            .unwrap()
            .unwrap()
            .state,
        "cancelled"
    );
    server.abort();
}
#[tokio::test]
async fn legacy_lock_contention_refuses_public_send_before_admission() {
    let (_dir, paths, input, now) = fixture();
    let mut legacy = Ledger::open(&paths).unwrap();
    let existing = legacy.accept(&input, now).unwrap();
    let events = legacy.events(None).unwrap();
    let mut refused = Vec::new();
    for mode in [PublicMode::Submit, PublicMode::Queue] {
        let mut b = binding(input.clone());
        b.request.message_id = uuid::Uuid::new_v4().to_string();
        b.request.mode = mode;
        let id = b.request.message_id.clone();
        let error = send_public(&paths, b.request).await.unwrap_err();
        assert_eq!(error.downcast_ref::<Fault>().unwrap().0, "adapter_busy");
        assert!(get(&legacy, &id).unwrap().is_none());
        assert!(legacy.get(&id).unwrap().is_none());
        refused.push(id);
    }
    assert_eq!(legacy.get(&input.message_id).unwrap(), Some(existing));
    assert_eq!(legacy.events(None).unwrap(), events);
    // Lock refusal precedes catalog resolution, discovery and native I/O.
    assert!(!paths.root.join(crate::layout::SESSION_CATALOG).exists());
    drop(legacy);
    for id in refused {
        let error = receipt_public(&paths, &id, false).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<Fault>().unwrap().0,
            "receipt_not_found"
        );
    }
    assert_eq!(Ledger::open(&paths).unwrap().events(None).unwrap(), events);
}
#[tokio::test]
async fn busy_receipt_lookup_does_not_negate_existing_admission() {
    let (_dir, paths, input, now) = fixture();
    let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    r.state = "native_accepted".into();
    r.native_queue_id = Some("queue-1".into());
    let b = binding(input.clone());
    save_binding(&paths, &b, &r);
    let legacy = Ledger::open(&paths).unwrap();
    let error = receipt_public(&paths, &input.message_id, false)
        .await
        .unwrap_err();
    assert_eq!(error.downcast_ref::<Fault>().unwrap().0, "adapter_busy");
    assert_eq!(legacy.get(&input.message_id).unwrap(), Some(r));
    assert!(get(&legacy, &input.message_id).unwrap().is_some());
    drop(legacy);
    let recovered = receipt_public(&paths, &input.message_id, false)
        .await
        .unwrap();
    assert_eq!(recovered["facts"]["accepted_native"], true);
    assert_eq!(recovered["native_queue_id"], "queue-1");
    assert_eq!(recovered["observation"]["state"], "unavailable");
}
#[test]
fn projection_preserves_distinct_ids_and_filters_non_public_fields() {
    let (_dir, _paths, input, _) = fixture();
    let mut b = binding(input);
    b.evidence_observed_at = Some(100);
    let out = wrap(
        &b,
        json!({"state":"completed_with_reply","native_queue_id":"queue-1","correlation":{"turn_id":"turn-1","user_item_id":"item-41","completed_user_item_id":"item-1","observed_generation":"c".repeat(64),"raw":"SECRET"},"reply":{"item_id":"reply-1","text":"Public answer","phase":"final_answer","reasoning":"SECRET"},"observation":{"state":"cached","raw":"SECRET"},"history":"SECRET"}),
    );
    assert_eq!(out["correlation"]["original_input_item_id"], "item-41");
    assert_eq!(out["correlation"]["completion_input_item_id"], "item-1");
    assert_eq!(out["correlation"]["reply_item_id"], "reply-1");
    assert_ne!(
        out["accepted_generation"],
        out["correlation"]["observed_runtime_generation"]
    );
    assert_eq!(out["observation"]["freshness"], "stale");
    assert_eq!(out["observation"]["evidence_observed_at"], 100);
    assert!(out["observation"]["native_attempted_at"].is_null());
    assert!(!out.to_string().contains("SECRET"));
}
#[tokio::test]
async fn public_projection_recovery_survives_reopen_and_expiry_without_sending() {
    let (_dir, paths, mut input, _) = fixture();
    let now = crate::daemon::now();
    input.expires_at = now - 100;
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let mid = input.message_id.clone();
    let tid = input.session_id.clone();
    let server=scripted_server(&a.endpoint(),move |req|{
        if req["method"]=="thread/read" {return Some(json!({"thread":{"id":tid,"path":null}}));}
        assert_eq!(req["method"],"thread/turns/list");assert_eq!(req["params"]["itemsView"],"summary");assert_eq!(req["params"]["threadId"],tid);
        Some(json!({"data":[{"id":"turn-1","status":"completed","itemsView":"summary","items":[{"type":"userMessage","id":"item-1","clientId":mid,"content":"SECRET"},{"type":"agentMessage","id":"reply-1","text":"Public answer","phase":"final_answer"}]}],"nextCursor":null}))
    }).await;
    input.expected_generation = a.generation();
    let b = binding(input.clone());
    let mut r = Ledger::open(&paths)
        .unwrap()
        .accept(&input, now - 200)
        .unwrap();
    r.state = "execution_started".into();
    r.native_queue_id = Some("queue-1".into());
    r.correlation = Some(observe::Correlation {
        turn_id: "turn-1".into(),
        user_item_id: "item-41".into(),
        completed_user_item_id: None,
        observed_generation: "b".repeat(64),
    });
    save_binding(&paths, &b, &r);
    let out = receipt_public(&paths, &input.message_id, false)
        .await
        .unwrap();
    assert_eq!(out["state"], "completed_with_reply");
    assert_eq!(out["correlation"]["original_input_item_id"], "item-41");
    assert_eq!(out["correlation"]["completion_input_item_id"], "item-1");
    assert_eq!(out["observation"]["state"], "observed");
    assert!(out["observation"]["evidence_observed_at"].is_i64());
    server.abort();
    let cached = receipt_public(&paths, &input.message_id, false)
        .await
        .unwrap();
    assert_eq!(cached["observation"]["state"], "cached");
    assert_eq!(
        cached["observation"]["evidence_observed_at"],
        out["observation"]["evidence_observed_at"]
    );
    assert_eq!(cached["correlation"], out["correlation"]);
    assert!(!cached.to_string().contains("SECRET"));
}
#[tokio::test]
async fn observer_errors_preserve_accepted_facts_and_old_receipts_are_not_adopted() {
    let (_dir, paths, input, now) = fixture();
    let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    r.state = "native_accepted".into();
    r.native_queue_id = Some("queue-1".into());
    Ledger::open(&paths).unwrap().save(&r).unwrap();
    assert!(
        receipt_public(&paths, &input.message_id, false)
            .await
            .is_err()
    );
    assert_eq!(
        Ledger::open(&paths)
            .unwrap()
            .get(&input.message_id)
            .unwrap(),
        Some(r.clone())
    );
    let b = binding(input.clone());
    save_binding(&paths, &b, &r);
    let out = receipt_public(&paths, &input.message_id, false)
        .await
        .unwrap();
    assert_eq!(out["state"], "native_accepted");
    assert_eq!(out["facts"]["accepted_native"], true);
    assert_eq!(out["observation"]["state"], "unavailable");
    assert!(out["observation"]["error_code"].is_string());
    assert!(out["observation"]["evidence_observed_at"].is_null());
}
fn event_count(paths: &Paths) -> usize {
    Ledger::open(paths).unwrap().events(None).unwrap()["events"]
        .as_array()
        .unwrap()
        .len()
}
/// Two receipt polls of a `native_accepted` receipt both fall back to the writer
/// path and observe natively: exactly one commits, and neither sends input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_observing_receipts_commit_once_and_never_send() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    input.expected_generation = a.generation();
    let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
    r.state = "native_accepted".into();
    r.native_queue_id = Some("queue-1".into());
    save_binding(&paths, &binding(input.clone()), &r);
    crate::messaging::TEST_WAIT
        .lock()
        .unwrap()
        .push((paths.root.clone(), std::time::Duration::from_secs(10)));
    let sends = std::sync::Arc::new(AtomicUsize::new(0));
    let reads = std::sync::Arc::new(AtomicUsize::new(0));
    let (s, rd) = (sends.clone(), reads.clone());
    let server = crate::messaging::tests::scripted_connections(
        &a.endpoint(),
        move |request| {
            let method = request["method"].as_str().unwrap_or_default();
            if method.starts_with("thread/queue") || method.starts_with("turn/") {
                s.fetch_add(1, Ordering::SeqCst);
                return Some(json!({"__error":true}));
            }
            if method == "thread/read" {
                return Some(json!({"thread":{"id":request["params"]["threadId"],"path":null}}));
            }
            rd.fetch_add(1, Ordering::SeqCst);
            Some(
                serde_json::from_str::<Value>(include_str!(
                    "../../../../docs/messaging/v1.2.1/fixtures.json"
                ))
                .unwrap()["native_summary"]
                    .clone(),
            )
        },
        2,
    )
    .await;
    let before = event_count(&paths);
    let (x, y) = tokio::join!(
        receipt_public(&paths, &input.message_id, false),
        receipt_public(&paths, &input.message_id, false)
    );
    let (x, y) = (x.unwrap(), y.unwrap());
    assert_eq!(event_count(&paths), before + 1, "{x}\n{y}");
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    assert!(reads.load(Ordering::SeqCst) >= 1);
    let stored = Ledger::open(&paths)
        .unwrap()
        .get(&input.message_id)
        .unwrap()
        .unwrap();
    assert_ne!(stored.state, "native_accepted");
    let observed = [&x, &y]
        .iter()
        .filter(|v| v["observation"]["state"] == "observed")
        .count();
    assert_eq!(observed, 1, "{x}\n{y}");
    server.abort();
}

/// The shared-reader shortcut of `receipt_public` answers exactly what the writer
/// path answers, writes nothing, and declines every case that may write or read
/// the provider.
#[tokio::test]
async fn receipt_shortcut_matches_the_writer_path_or_declines() {
    let modes = [
        PublicMode::Submit,
        PublicMode::Queue,
        PublicMode::Resume,
        PublicMode::Steer,
    ];
    let states = [
        "pending_local",
        "unknown",
        "native_accepted",
        "execution_started",
        "completed_with_reply",
        "completed_without_reply",
        "failed",
        "rejected",
        "cancelled",
        "expired",
    ];
    let short = |paths: &Paths, id: &str, cancelling: bool| {
        let reader = ledger::open_read_blocking(paths, std::time::Duration::ZERO).unwrap();
        settled_receipt(paths, &reader, id, cancelling)
    };
    let mut answered = 0;
    for mode in &modes {
        for state in states {
            for cancelling in [false, true] {
                let (_dir, paths, input, now) = fixture();
                let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
                r.state = state.into();
                let mut b = binding(input.clone());
                b.request.mode = mode.clone();
                save_binding(&paths, &b, &r);
                let got = short(&paths, &input.message_id, cancelling).unwrap();
                let declines = cancelling
                    || matches!(mode, PublicMode::Steer | PublicMode::Resume)
                    || matches!(
                        state,
                        "pending_local" | "unknown" | "native_accepted" | "execution_started"
                    );
                assert_eq!(
                    got.is_none(),
                    declines,
                    "{mode:?} {state} {cancelling}: {got:?}"
                );
                if let Some(got) = got {
                    let before = event_count(&paths);
                    let writer =
                        receipt_write(&paths, &input.message_id, false, std::time::Duration::ZERO)
                            .await
                            .unwrap();
                    assert_eq!(got, writer, "{mode:?} {state}");
                    assert_eq!(event_count(&paths), before, "{mode:?} {state}");
                    answered += 1;
                }
            }
        }
    }
    assert_eq!(answered, 2 * 6);
    // Ids without a receipt row: unknown, cancelled binding, undispatched binding.
    let (_dir, paths, input, _) = fixture();
    drop(Ledger::open(&paths).unwrap());
    let unknown = uuid::Uuid::new_v4().to_string();
    let s = short(&paths, &unknown, false).unwrap_err().to_string();
    let w = receipt_write(&paths, &unknown, false, std::time::Duration::ZERO)
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        (s.as_str(), w.as_str()),
        ("receipt_not_found", "receipt_not_found")
    );
    for cancelled in [false, true] {
        let mut b = binding(input.clone());
        b.request.message_id = uuid::Uuid::new_v4().to_string();
        b.cancelled = cancelled;
        let id = b.request.message_id.clone();
        Ledger::open(&paths)
            .unwrap()
            .db
            .execute(
                "INSERT INTO public_messages VALUES(?,?)",
                params![id, serde_json::to_string(&b).unwrap()],
            )
            .unwrap();
        let got = short(&paths, &id, false).unwrap().unwrap();
        let writer = receipt_write(&paths, &id, false, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(got, writer, "cancelled={cancelled}");
        if !cancelled {
            assert!(short(&paths, &id, true).unwrap().is_none());
        }
    }
}
