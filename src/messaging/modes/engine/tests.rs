use super::*;
fn req() -> Request {
    serde_json::from_value(
        serde_json::from_str::<Value>(include_str!("../../../../tests/fixtures/legacy-modes.json"))
            .unwrap()["request"]
            .clone(),
    )
    .unwrap()
}
fn snap() -> Snapshot {
    serde_json::from_value(
        serde_json::from_str::<Value>(include_str!("../../../../tests/fixtures/legacy-modes.json"))
            .unwrap()["snapshot"]
            .clone(),
    )
    .unwrap()
}
#[test]
fn exclusive_claim_restart_expiry_and_cancel_are_durable() {
    let d = tempfile::tempdir().unwrap();
    let j = Journal::open(d.path()).unwrap();
    assert_eq!(Journal::open(d.path()).err().unwrap().0, "adapter_busy");
    let r = req();
    j.accept(&r, r.created_at).unwrap();
    let (out, call) = j.dispatch(&r, r.expires_at, &snap(), "ack").unwrap();
    assert_eq!(out.state, "expired");
    assert!(call.is_none());
    drop(j);
    let j = Journal::open(d.path()).unwrap();
    assert_eq!(j.accept(&r, r.expires_at + 100).unwrap().state, "expired");
    let mut r = r;
    r.request_id = uuid::Uuid::new_v4().to_string();
    j.accept(&r, r.created_at).unwrap();
    assert_eq!(j.cancel(&r.request_id).unwrap().state, "cancelled");
    assert!(
        j.dispatch(&r, r.created_at, &snap(), "ack")
            .unwrap()
            .1
            .is_none()
    );
}
#[test]
fn bounded_backlog_does_not_evict_idempotency_keys() {
    let d = tempfile::tempdir().unwrap();
    let j = Journal::open(d.path()).unwrap();
    let mut r = req();
    for _ in 0..1000 {
        r.request_id = uuid::Uuid::new_v4().to_string();
        j.accept(&r, r.created_at).unwrap();
    }
    assert!(j.accept(&r, r.created_at).is_ok());
    r.request_id = uuid::Uuid::new_v4().to_string();
    assert_eq!(j.accept(&r, r.created_at).unwrap_err().0, "backpressure");
}
#[test]
fn migration_waits_then_rejects_changed_generation_and_other_session_replies() {
    let d = tempfile::tempdir().unwrap();
    let j = Journal::open(d.path()).unwrap();
    let r = req();
    j.accept(&r, r.created_at).unwrap();
    let mut s = snap();
    s.state = "migrating".into();
    assert_eq!(
        j.dispatch(&r, r.created_at, &s, "ack").unwrap().0.state,
        "waiting"
    );
    s.state = "idle".into();
    s.generation = "b".repeat(64);
    let (out, call) = j.dispatch(&r, r.created_at, &s, "ack").unwrap();
    assert_eq!(out.reason.as_deref(), Some("generation_mismatch"));
    assert!(call.is_none());
    assert_eq!(
        j.observe(
            &r.request_id,
            "different-session",
            "turn-a",
            &r.request_id,
            "reply_observed",
            "public-1",
            Some("reply".into())
        )
        .unwrap_err()
        .0,
        "correlation_changed"
    );
}
#[test]
fn expired_unsent_head_does_not_block_later_queue_request() {
    let d = tempfile::tempdir().unwrap();
    let j = Journal::open(d.path()).unwrap();
    let mut old = req();
    old.mode = Mode::Queue;
    old.expires_at = old.created_at + 1;
    j.accept(&old, old.created_at).unwrap();
    let mut next = req();
    next.mode = Mode::Queue;
    next.request_id = uuid::Uuid::new_v4().to_string();
    j.accept(&next, next.created_at).unwrap();
    let (out, call) = j
        .dispatch(&next, next.created_at + 2, &snap(), "ack")
        .unwrap();
    assert_eq!(out.state, "native_accepted");
    assert!(call.is_some());
    assert_eq!(j.get(&old.request_id).unwrap().unwrap().state, "expired");
}
