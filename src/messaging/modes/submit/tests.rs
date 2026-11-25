use super::*;
#[tokio::test]
async fn crashed_submit_intent_is_atomic_and_never_reclaimed_or_misread_as_queue() {
    let (_tmp, paths, input, now) = super::super::super::tests::fixture();
    let request = Request {
        request_id: input.message_id.clone(),
        session_id: input.session_id.clone(),
        mode: Mode::Submit,
        text: input.text.clone(),
        expected_generation: input.expected_generation.clone(),
        expected_turn_id: Some("turn-a".into()),
        expected_previous_turn_id: None,
        expected_stop_id: None,
        resume: false,
        capability_id: capability_id(&input.expected_generation),
        created_at: now,
        expires_at: input.expires_at,
    };
    {
        let mut ledger = Ledger::open(&paths).unwrap();
        let receipt = ledger
            .accept_bound(
                &input,
                now,
                Some((
                    &request.fingerprint().unwrap(),
                    "submit",
                    Some(SubmitEvidence {
                        expected_turn_id: "turn-a".into(),
                        acknowledged_turn_id: None,
                        action: None,
                        resume: false,
                    }),
                )),
            )
            .unwrap();
        assert_eq!(receipt.state, "unknown");
        assert_eq!(receipt.submit.unwrap().expected_turn_id, "turn-a");
        assert!(Ledger::open(&paths).is_err());
    }
    // No target exists: retry/status must use only persisted intent.
    let repeated = queue::request(&paths, request.clone()).await.unwrap();
    assert_eq!(repeated["state"], "delivery_unknown");
    assert_eq!(repeated["expected_turn_id"], "turn-a");
    assert_eq!(
        queue::status(&paths, &request.request_id).await.unwrap(),
        repeated
    );
    let mut ledger = Ledger::open(&paths).unwrap();
    assert_eq!(
        ledger.accept(&input, now).unwrap_err().0,
        "message_id_conflict"
    );
    drop(ledger);
    let mut changed = request;
    changed.mode = Mode::Queue;
    changed.expected_turn_id = None;
    assert_eq!(
        queue::request(&paths, changed).await.unwrap_err().0,
        "idempotency_conflict"
    );
}

#[test]
fn published_submit_receipt_matches_actual_projection() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../docs/messaging/modes-v1/submit-fixtures.json"
    ))
    .unwrap();
    assert_eq!(wrap(fixture["stored_receipt"].clone()), fixture["result"]);
    let request: Request = serde_json::from_value(fixture["request"].clone()).unwrap();
    assert_eq!(
        request.capability_id,
        capability_id(&request.expected_generation)
    );
}
