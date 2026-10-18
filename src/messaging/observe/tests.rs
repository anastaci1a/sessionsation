use super::*;
use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_server};
fn shared() -> Value {
    serde_json::from_str(include_str!("../../../docs/messaging/v1.2.1/fixtures.json")).unwrap()
}
fn receipt(paths: &Paths, input: &Enqueue, now: i64) -> Receipt {
    let mut ledger = Ledger::open(paths).unwrap();
    let mut receipt = ledger.accept(input, now).unwrap();
    receipt.state = "unknown".into();
    ledger.save(&receipt).unwrap();
    receipt
}
async fn projected(page: Value) -> Result<Option<Receipt>> {
    let (_dir, paths, input, now) = fixture();
    let a = attachment_fixture(&paths, &input);
    let r = receipt(&paths, &input, now);
    let server = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/turns/list");
        assert_eq!(req["params"]["itemsView"], "summary");
        Some(page.clone())
    })
    .await;
    let mut control = a.connect().await.unwrap();
    let outcome = read(&mut control, &r, &input.expected_generation).await;
    server.abort();
    outcome
}
#[tokio::test]
async fn public_request_id_correlates_terminal_reply_using_shared_fixture() {
    let observed = projected(shared()["native_summary"].clone())
        .await
        .unwrap()
        .unwrap();
    let mut expected = shared()["completed_receipt"].clone();
    expected["native_queue_id"] = Value::Null;
    assert_eq!(json!(observed), expected);
}
#[tokio::test]
async fn retry_after_lost_ack_restart_and_event_replay_never_resends() {
    let (_dir, paths, mut input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let page = shared()["native_summary"].clone();
    let server = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/turns/list");
        Some(page.clone())
    })
    .await;
    input.expected_generation = a.generation();
    receipt(&paths, &input, now);
    // New ledger/transport, like a restarted adapter after unknown native acceptance.
    let mut ledger = Ledger::open(&paths).unwrap();
    let mut r = ledger.accept(&input, now + 1).unwrap();
    assert_eq!(r.state, "unknown");
    let value = result(&paths, &mut ledger, &mut r).await.unwrap();
    assert_eq!(value["observation"]["state"], "observed");
    assert_eq!(r.state, "completed_with_reply");
    let events = ledger.events(None).unwrap();
    let cursor = events["cursor"].as_str().unwrap().to_owned();
    drop(ledger);
    server.abort();
    let mut ledger = Ledger::open(&paths).unwrap();
    let mut retried = ledger.accept(&input, now + 2).unwrap();
    let value = result(&paths, &mut ledger, &mut retried).await.unwrap();
    assert_eq!(value["observation"]["state"], "cached");
    assert_eq!(value["reply"]["text"], "Visible answer.");
    assert_eq!(ledger.events(None).unwrap(), events);
    assert_eq!(ledger.events(Some(&cursor)).unwrap()["events"], json!([]));
    let mut changed = input.clone();
    changed.text = "conflicting".into();
    assert_eq!(
        ledger.accept(&changed, now + 2).unwrap_err().0,
        "message_id_conflict"
    );
}
#[tokio::test]
async fn missing_duplicate_and_changed_client_ids_never_guess_a_reply() {
    let mut missing = shared()["native_summary"].clone();
    missing["data"][0]["items"][0]["clientId"] = Value::Null;
    assert!(projected(missing).await.unwrap().is_none());
    let mut other = shared()["native_summary"].clone();
    other["data"][0]["items"][0]["clientId"] = json!("another-request");
    assert!(projected(other).await.unwrap().is_none());
    let mut duplicate = shared()["native_summary"].clone();
    let item = duplicate["data"][0].clone();
    duplicate["data"].as_array_mut().unwrap().push(item);
    assert_eq!(
        projected(duplicate).await.unwrap_err().0,
        "ambiguous_native_correlation"
    );
}
#[tokio::test]
async fn only_terminal_public_text_and_supported_summary_are_accepted() {
    for (field, value, error) in [
        ("phase", json!("commentary"), "reply_not_final"),
        ("text", json!("x".repeat(65537)), "reply_too_large"),
    ] {
        let mut page = shared()["native_summary"].clone();
        page["data"][0]["items"][1][field] = value;
        assert_eq!(projected(page).await.unwrap_err().0, error);
    }
    let mut page = shared()["native_summary"].clone();
    page["data"][0]["itemsView"] = json!("full");
    assert_eq!(projected(page).await.unwrap_err().0, "unsupported_summary");
    let mut page = shared()["native_summary"].clone();
    page["data"][0]["items"][1] = json!({"type":"reasoning","text":"SECRET_REASONING"});
    assert_eq!(projected(page).await.unwrap_err().0, "unsupported_summary");
    let mut page = shared()["native_summary"].clone();
    page["data"][0]["items"][1]["phase"] = Value::Null;
    assert_eq!(
        projected(page).await.unwrap().unwrap().reply.unwrap().phase,
        None
    );
}
#[tokio::test]
async fn started_then_completed_is_monotonic_and_deduplicated() {
    let mut page = shared()["native_summary"].clone();
    page["data"][0]["status"] = json!("inProgress");
    let started = projected(page).await.unwrap().unwrap();
    assert!(started.reply.is_none());
    assert_eq!(started.state, "execution_started");
    let completed = projected(shared()["native_summary"].clone())
        .await
        .unwrap()
        .unwrap();
    let (_dir, paths, input, now) = fixture();
    let mut ledger = Ledger::open(&paths).unwrap();
    let mut current = ledger.accept(&input, now).unwrap();
    commit(&mut ledger, &mut current, started.clone()).unwrap();
    commit(&mut ledger, &mut current, completed.clone()).unwrap();
    let events = ledger.events(None).unwrap();
    commit(&mut ledger, &mut current, started).unwrap();
    commit(&mut ledger, &mut current, completed).unwrap();
    assert_eq!(ledger.events(None).unwrap(), events);
    assert_eq!(current.state, "completed_with_reply");
}
#[tokio::test]
async fn incomplete_pagination_and_unsupported_native_never_publish_partial_match() {
    let mut page = shared()["native_summary"].clone();
    page["nextCursor"] = json!("repeated");
    page["data"] = json!([]);
    assert_eq!(
        projected(page).await.unwrap_err().0,
        "reply_scan_incomplete"
    );
    let (_dir, paths, input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let mut r = receipt(&paths, &input, now);
    let mut ledger = Ledger::open(&paths).unwrap();
    let value = result(&paths, &mut ledger, &mut r).await.unwrap();
    assert_eq!(value["observation"]["code"], "endpoint_unavailable");
    assert!(r.reply.is_none());
    assert_eq!(r.state, "unknown");
}
#[tokio::test]
async fn generation_change_and_migration_do_not_retarget_writes() {
    let (_dir, paths, input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let page = shared()["native_summary"].clone();
    let binding = paths.root.join("managed-panes/1.json");
    let server = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/turns/list");
        fs::remove_file(&binding).unwrap();
        Some(page.clone())
    })
    .await;
    let mut r = receipt(&paths, &input, now);
    let mut ledger = Ledger::open(&paths).unwrap();
    let value = result(&paths, &mut ledger, &mut r).await.unwrap();
    assert_eq!(value["observation"]["state"], "unavailable");
    assert!(r.reply.is_none());
    server.abort();
}
#[tokio::test]
async fn new_attachment_can_recover_same_logical_request_without_resubmission() {
    let (_dir, paths, input, now) = fixture();
    let mut a = attachment_fixture(&paths, &input);
    bind(&paths, &mut a);
    let mut page = shared()["native_summary"].clone();
    page["private"] = json!("SECRET_TOKEN");
    page["data"][0]["error"] = json!({"message":"SECRET_TOOL"});
    page["data"][0]["items"][0]["content"] = json!([{"text":"SECRET_USER"}]);
    let server = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/turns/list");
        Some(page.clone())
    })
    .await;
    let mut r = receipt(&paths, &input, now);
    let mut ledger = Ledger::open(&paths).unwrap();
    let response = result(&paths, &mut ledger, &mut r).await.unwrap();
    assert_ne!(
        r.generation,
        r.correlation.as_ref().unwrap().observed_generation
    );
    assert_eq!(r.generation, input.expected_generation);
    assert!(!response.to_string().contains("SECRET"));
    assert!(!ledger.events(None).unwrap().to_string().contains("SECRET"));
    assert!(
        !String::from_utf8_lossy(&fs::read(paths.root.join("messaging/receipts.db")).unwrap())
            .contains("SECRET")
    );
    server.abort();
}

#[tokio::test]
async fn failed_turn_does_not_export_native_error_or_claim_completion() {
    let mut page = shared()["native_summary"].clone();
    page["data"][0]["status"] = json!("failed");
    page["data"][0]["error"] = json!({"message":"PRIVATE_FAILURE"});
    let receipt = projected(page).await.unwrap().unwrap();
    assert_eq!(receipt.state, "failed");
    assert!(receipt.reply.is_none());
    assert!(!json!(receipt).to_string().contains("PRIVATE_FAILURE"));
}

#[tokio::test]
async fn recorded_correlation_cannot_change_and_full_reads_are_forbidden() {
    let (_dir, paths, input, now) = fixture();
    let a = attachment_fixture(&paths, &input);
    let mut r = receipt(&paths, &input, now);
    r.correlation = Some(Correlation {
        turn_id: "another-turn".into(),
        user_item_id: "user-1".into(),
        completed_user_item_id: None,
        observed_generation: input.expected_generation.clone(),
    });
    let page = shared()["native_summary"].clone();
    let server = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/turns/list");
        Some(page.clone())
    })
    .await;
    let mut control = a.connect().await.unwrap();
    assert_eq!(
        control
            .request::<Value>("thread/turns/list", json!({"itemsView":"full"}))
            .await
            .unwrap_err()
            .0,
        "unsupported"
    );
    assert_eq!(
        read(&mut control, &r, &input.expected_generation)
            .await
            .unwrap_err()
            .0,
        "correlation_changed"
    );
    server.abort();
}

async fn read_existing(r: &Receipt, page: Value) -> Result<Option<Receipt>> {
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let server = scripted_server(&a.endpoint(), move |req| {
        assert_eq!(req["method"], "thread/turns/list");
        assert_eq!(req["params"]["itemsView"], "summary");
        assert_eq!(req["params"]["threadId"], page["session_fixture_id"]);
        Some(page.clone())
    })
    .await;
    let mut control = a.connect().await.unwrap();
    let value = read(&mut control, r, &input.expected_generation).await;
    server.abort();
    value
}
fn reindexed_page(input: &Enqueue) -> Value {
    let mut page = shared()["reindexed_summary"].clone();
    page["session_fixture_id"] = json!(input.session_id);
    page
}

#[tokio::test]
async fn legacy_summary_reindex_recovers_after_restart_and_deadline_once() {
    let (_dir, paths, input, now) = fixture();
    let mut r = receipt(&paths, &input, now);
    let mut live = reindexed_page(&input);
    live["data"][0]["status"] = json!("inProgress");
    live["data"][0]["items"][0]["id"] = json!("item-1");
    let started = read_existing(&r, live).await.unwrap().unwrap();
    {
        let mut ledger = Ledger::open(&paths).unwrap();
        commit(&mut ledger, &mut r, started).unwrap();
    }
    let mut ledger = Ledger::open(&paths).unwrap();
    let mut r = ledger.get(&input.message_id).unwrap().unwrap();
    // Existing 1.2 receipts decode without the new optional field. Expiry is
    // a submission deadline, never a reason to discard accepted execution.
    ledger.expire(&mut r, input.expires_at + 1000).unwrap();
    assert_eq!(r.state, "execution_started");
    let completed = read_existing(&r, reindexed_page(&input))
        .await
        .unwrap()
        .unwrap();
    let mut expected = shared()["reindexed_receipt"].clone();
    expected["native_queue_id"] = Value::Null;
    assert_eq!(json!(completed), expected);
    commit(&mut ledger, &mut r, completed.clone()).unwrap();
    let events = ledger.events(None).unwrap();
    commit(&mut ledger, &mut r, completed).unwrap();
    assert_eq!(ledger.events(None).unwrap(), events);
    drop(ledger);
    let mut ledger = Ledger::open(&paths).unwrap();
    let mut r = ledger.get(&input.message_id).unwrap().unwrap();
    let cached = result(&paths, &mut ledger, &mut r).await.unwrap();
    assert_eq!(cached["observation"]["state"], "cached");
    assert_eq!(cached["reply"]["text"], "Visible answer.");
    assert_eq!(ledger.events(None).unwrap(), events);
    assert_eq!(
        events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["receipt"]["state"] == "completed_with_reply")
            .count(),
        1
    );
}

#[tokio::test]
async fn reindex_never_accepts_changed_turn_client_nonlegacy_or_ambiguous_evidence() {
    let (_dir, paths, input, now) = fixture();
    let mut r = receipt(&paths, &input, now);
    r.state = "execution_started".into();
    r.correlation = Some(Correlation {
        turn_id: "turn-1".into(),
        user_item_id: "item-1".into(),
        completed_user_item_id: None,
        observed_generation: input.expected_generation.clone(),
    });
    for (field, value) in [("id", "different-turn"), ("status", "inProgress")] {
        let mut page = reindexed_page(&input);
        page["data"][0][field] = json!(value);
        assert_eq!(
            read_existing(&r, page).await.unwrap_err().0,
            "correlation_changed"
        );
    }
    for id in ["user-110", "item-0", "item-0110", "item--1"] {
        let mut page = reindexed_page(&input);
        page["data"][0]["items"][0]["id"] = json!(id);
        assert_eq!(
            read_existing(&r, page).await.unwrap_err().0,
            "correlation_changed"
        );
    }
    let mut page = reindexed_page(&input);
    page["data"][0]["items"][0]["clientId"] = json!("another-message");
    assert!(read_existing(&r, page).await.unwrap().is_none());
    let mut page = reindexed_page(&input);
    let duplicate = page["data"][0].clone();
    page["data"].as_array_mut().unwrap().push(duplicate);
    assert_eq!(
        read_existing(&r, page).await.unwrap_err().0,
        "ambiguous_native_correlation"
    );
    let mut other = r.clone();
    other.state = "native_accepted".into();
    assert_eq!(
        read_existing(&other, reindexed_page(&input))
            .await
            .unwrap_err()
            .0,
        "correlation_changed"
    );
    other = r.clone();
    other.correlation.as_mut().unwrap().user_item_id = "user-1".into();
    assert_eq!(
        read_existing(&other, reindexed_page(&input))
            .await
            .unwrap_err()
            .0,
        "correlation_changed"
    );
}
