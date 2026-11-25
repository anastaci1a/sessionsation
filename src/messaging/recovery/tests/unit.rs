#[tokio::test]
async fn terminal_readiness_leads_to_exact_item_recovery_without_replay() {
    use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_connections};
    for (state, last, expected) in [
        ("idle", "interrupted", "interrupted"),
        ("idle", "failed", "errored"),
        ("systemError", "failed", "errored"),
    ] {
        let (_dir, paths, mut input, _) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        input.expires_at = crate::daemon::now() + 120;
        let id = a.id.clone();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let server = scripted_connections(&a.endpoint(), move |r| {
            let method = r["method"].as_str().unwrap();
            seen.lock().unwrap().push(method.to_owned());
            match method {
                "thread/read" => Some(json!({"thread":{"id":id,"status":{"type":state}}})),
                "thread/turns/list" => Some(json!({"data":[{"id":"previous","status":last}]})),
                "thread/queue/list" => Some(json!({"data":[],"nextCursor":null})),
                "thread/queue/add" => Some(json!({"queuedSubmission":{"id":"exact-item","clientUserMessageId":r["params"]["clientUserMessageId"]}})),
                "thread/queue/start" => {
                    assert_eq!(r["params"]["queuedSubmissionId"], "exact-item");
                    Some(json!({"turn":{"id":"continued","status":"inProgress"}}))
                }
                other => panic!("Unexpected RPC: {other}"),
            }
        }, 3).await;
        let instance: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"fixture","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
        let session = crate::catalog::reconcile(&paths, &[instance])
            .unwrap()
            .remove(0);
        let observation = readiness::observe_public(&paths, &session.id, &session.generation())
            .await
            .unwrap();
        assert_eq!(observation["state"], expected);
        for mode in ["submit", "queue"] {
            // A failed turn leaves a live idle session that takes ordinary input;
            // only an interrupted one needs explicit Resume.
            if expected == "errored" {
                assert_eq!(observation["actions"][mode]["available"], true);
                continue;
            }
            assert_eq!(
                observation["actions"][mode]["reason"],
                "explicit_resume_required"
            );
            assert_eq!(observation["actions"][mode]["available"], false);
        }
        assert_eq!(observation["actions"]["resume"]["available"], true);
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .all(|m| m == "thread/read" || m == "thread/turns/list")
        );
        let request = PublicRequest {
            origin: None,
            delivery: None,
            await_reply: false,
            if_busy: None,
            api_version: 1,
            message_id: input.message_id.clone(),
            session_id: session.id,
            expected_generation: observation["generation"].as_str().unwrap().into(),
            mode: PublicMode::Resume,
            text: input.text.clone(),
            expires_at: input.expires_at,
            steering: None,
            recovery: Some(Intent {
                expected_state: observation["runtime_state"].as_str().unwrap().into(),
                expected_previous_turn_id: serde_json::from_value(
                    observation["previous_turn_id"].clone(),
                )
                .unwrap(),
                expected_stop_id: serde_json::from_value(
                    observation["explicit_intent"]["id"].clone(),
                )
                .unwrap(),
                expected_runtime_generation: serde_json::from_value(
                    observation["runtime_generation"].clone(),
                )
                .unwrap(),
                launch: None,
            }),
        };
        input.expected_generation = a.generation();
        let mut ledger = Ledger::open(&paths).unwrap();
        let mut receipt = ledger.accept(&input, crate::daemon::now()).unwrap();
        deliver(&paths, &mut ledger, &mut receipt, &input, &request)
            .await
            .unwrap();
        assert_eq!(receipt.state, "native_accepted");
        assert_eq!(receipt.recovery_start.as_ref().unwrap().state, "confirmed");
        assert!(receipt.correlation.is_none()); // Start acknowledgement is not consumption.
        drop(ledger);
        let mut ledger = Ledger::open(&paths).unwrap();
        let mut receipt = ledger.get(&input.message_id).unwrap().unwrap();
        deliver(&paths, &mut ledger, &mut receipt, &input, &request)
            .await
            .unwrap();
        for method in ["thread/queue/add", "thread/queue/start"] {
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| m.as_str() == method)
                    .count(),
                1
            );
        }
        server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_legacy_summary_shares_the_bounded_recovery_preflight_budget() {
    use crate::messaging::tests::{attachment_fixture, fixture, scripted_server};
    let (_dir, paths, input, _) = fixture();
    let a = attachment_fixture(&paths, &input);
    let id = a.id.clone();
    let server = scripted_server(&a.endpoint(), move |r| {
        match r["method"].as_str().unwrap() {
            "thread/read" => Some(json!({"thread":{"id":id,"status":{"type":"idle"}}})),
            "thread/turns/list" => {
                // Must exceed the generic 3s per-call budget but stay well inside the 5s shared preflight budget; 3.1s maximises that margin.
                std::thread::sleep(std::time::Duration::from_millis(3100));
                Some(json!({"data":[{"id":"previous","status":"interrupted"}]}))
            }
            "thread/queue/list" => Some(json!({"data":[],"nextCursor":null})),
            _ => panic!("Preflight must not write"),
        }
    })
    .await;
    let mut control = a.connect().await.unwrap();
    let result = native_preflight(&mut control, &a.id, None, false)
        .await
        .unwrap();
    assert_eq!(result["previous_turn_id"], "previous");
    server.abort();
}
use super::super::*;
#[tokio::test]
async fn interrupted_worker_reconciliation_never_reclaims_a_launch_or_expired_message() {
    let (_dir, paths, input, _) = super::super::super::tests::fixture();
    let session = crate::catalog::Session {
        id: uuid::Uuid::new_v4().to_string(),
        name: "recovery".into(),
        provider: "codex".into(),
        account: "selected".into(),
        conversation_id: input.session_id,
        revision: 1,
        attachment: None,
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
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
        mode: PublicMode::Resume,
        text: "fixture".into(),
        expires_at: 1,
        recovery: Some(Intent {
            expected_state: "stopped".into(),
            expected_previous_turn_id: None,
            expected_stop_id: None,
            expected_runtime_generation: None,
            launch: None,
        }),
    };
    let mut record = Record {
        schema_version: 1,
        request: request.clone(),
        session,
        phase: "preparing".into(),
        detail: None,
        target: None,
        launched_generation: None,
        created_at: 1,
        updated_at: 1,
        launch_confirmed: false,
    };
    save(&paths, &record).unwrap();
    let r = reconcile_recovery(&paths, &request.message_id)
        .await
        .unwrap();
    assert_eq!(r["state"], "rejected");
    assert_eq!(r["facts"]["accepted_native"], false);
    let retry = send(&paths, request.clone()).await.unwrap();
    assert_eq!(retry["state"], "rejected");
    record.phase = "dispatching".into();
    save(&paths, &record).unwrap();
    assert!(
        reconcile_recovery(&paths, &request.message_id)
            .await
            .unwrap_err()
            .to_string()
            .contains("RECOVERY_LAUNCH_UNCERTAIN")
    );
    assert_eq!(
        read(&paths, &request.message_id).unwrap().phase,
        "dispatching"
    );
    assert!(
        crate::lifecycle::pending(&paths, &record.session.conversation_id, None)
            .unwrap()
            .is_some()
    );
    let mut changed = request;
    changed.text = "changed".into();
    assert!(
        send(&paths, changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("MESSAGE_ID_CONFLICT")
    );
}
