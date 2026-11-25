use super::*;
/// `modes` status: the shared-reader shortcut answers what the writer path answers
/// and writes nothing; unsettled and expiring receipts take the writer path.
#[tokio::test]
async fn status_shortcut_matches_the_writer_path_or_declines() {
    let mut answered = 0;
    for state in crate::messaging::tests::SHORTCUT_STATES {
        for expires in [false, true] {
            let (_dir, paths, mut input, _) = super::super::super::tests::fixture();
            let now = crate::daemon::now() - if expires { 3600 } else { 0 };
            input.expires_at = now + 600;
            let mut db = Ledger::open(&paths).unwrap();
            let mut r = db.accept_with_mode(&input, now, Some("mode-hash")).unwrap();
            r.state = state.into();
            db.save(&r).unwrap();
            drop(db);
            let reader = ledger::open_read_blocking(&paths, std::time::Duration::ZERO).unwrap();
            let stored = receipt(&reader, &input.message_id).unwrap();
            let got = observe::settled(&reader, &stored, crate::daemon::now()).unwrap();
            drop(reader);
            let unsettled = matches!(state, "unknown" | "native_accepted" | "execution_started");
            let expiring = state == "pending_local" && expires;
            assert_eq!(
                got.is_none(),
                unsettled || expiring,
                "{state} {expires}: {got:?}"
            );
            if let Some(got) = got {
                let before = Ledger::open(&paths).unwrap().events(None).unwrap();
                let writer = status_write(&paths, &input.message_id, std::time::Duration::ZERO)
                    .await
                    .unwrap();
                assert_eq!(wrap(got), writer, "{state}");
                assert_eq!(Ledger::open(&paths).unwrap().events(None).unwrap(), before);
                answered += 1;
            }
        }
    }
    assert_eq!(answered, 2 * 7 - 1);
}
#[tokio::test]
async fn atomic_mode_intent_survives_restart_without_dispatch_and_legacy_cannot_reuse_it() {
    let (_dir, paths, input, now) = super::super::super::tests::fixture();
    let r = Request {
        request_id: input.message_id.clone(),
        session_id: input.session_id.clone(),
        mode: Mode::Queue,
        text: input.text.clone(),
        expected_generation: input.expected_generation.clone(),
        expected_turn_id: None,
        expected_previous_turn_id: None,
        expected_stop_id: None,
        resume: false,
        capability_id: capability_id(&input.expected_generation),
        created_at: now,
        expires_at: input.expires_at,
    };
    let first = {
        let mut db = Ledger::open(&paths).unwrap();
        let receipt = db
            .accept_with_mode(&input, now, Some(&r.fingerprint().unwrap()))
            .unwrap();
        assert_eq!(receipt.state, "unknown");
        assert!(db.mode_record(&r.request_id).unwrap());
        assert!(Ledger::open(&paths).is_err());
        receipt
    };
    // No attachment or endpoint exists; a retry must be a lookup and still succeed.
    let out = request(&paths, r.clone()).await.unwrap();
    assert_eq!(out["delivery"], json!(first));
    let mut changed = r.clone();
    changed.mode = Mode::Submit;
    assert_eq!(
        request(&paths, changed).await.unwrap_err().0,
        "idempotency_conflict"
    );
    let mut db = Ledger::open(&paths).unwrap();
    assert_eq!(db.accept(&input, now).unwrap_err().0, "message_id_conflict");
}
#[test]
fn emergency_gate_is_shared_serialized_and_persistent() {
    let (_dir, paths, _, _) = super::super::super::tests::fixture();
    assert!(sending_enabled(&paths).unwrap());
    sending(&paths, false, true).unwrap();
    assert!(!sending_enabled(&paths).unwrap());
    let db = Ledger::open(&paths).unwrap();
    assert_eq!(sending(&paths, true, false).unwrap_err().0, "adapter_busy");
    drop(db);
    sending(&paths, true, false).unwrap();
    assert!(sending_enabled(&paths).unwrap());
}

#[test]
fn shared_native_queue_receipt_fixture_matches_projection() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../docs/messaging/modes-v1/queue-fixtures.json"
    ))
    .unwrap();
    assert_eq!(
        wrap(fixture["native_delivery"].clone()),
        fixture["native_result"]["result"]
    );
    let request: Request = serde_json::from_value(fixture["request"].clone()).unwrap();
    assert_eq!(
        request.capability_id,
        capability_id(&request.expected_generation)
    );
}
