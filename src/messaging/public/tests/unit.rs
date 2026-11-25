#[test]
fn codex_send_accepts_claude_parity_fields_and_names_unknown_ones() {
    use super::super::*;
    let base = json!({"api_version":1,"message_id":"m","session_id":"s","expected_generation":"g",
        "mode":"steer","text":"t","expires_at":1});
    let mut v = base.clone();
    v["origin"] = json!("agent");
    v["delivery"] = json!("auto");
    v["await_reply"] = json!(true);
    let r: PublicRequest = serde_json::from_value(v).unwrap();
    assert_eq!(r.origin.as_deref(), Some("agent"));
    assert!(check_codex_options(&r).is_ok());
    let mut native = r.clone();
    native.delivery = Some("native".into());
    let e = check_codex_options(&native).unwrap_err().to_string();
    assert!(e.starts_with("PROVIDER_DELIVERY_UNSUPPORTED"), "{e}");
    let mut bad = r.clone();
    bad.origin = Some("robot".into());
    assert!(
        check_codex_options(&bad)
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_REQUEST")
    );
    let mut v = base;
    v["bogus_field"] = json!(1);
    let e = serde_json::from_value::<PublicRequest>(v)
        .unwrap_err()
        .to_string();
    assert!(e.contains("bogus_field"), "{e}");
}
use super::super::*;
#[test]
fn public_receipt_distinguishes_native_acceptance_and_completion() {
    let b = Binding {
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
            message_id: uuid::Uuid::new_v4().to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            expected_generation: "a".repeat(64),
            recovery: None,
            mode: PublicMode::Submit,
            text: "private prompt".into(),
            expires_at: 100,
        },
        native: Enqueue {
            message_id: "m".into(),
            session_id: "native".into(),
            text: "private prompt".into(),
            expires_at: 100,
            expected_generation: "b".repeat(64),
        },
    };
    let out = wrap(
        &b,
        json!({"state":"unknown","reason":"submission_uncertain"}),
    );
    assert_eq!(out["state"], "delivery_unknown");
    assert_eq!(out["facts"]["accepted_native"], false);
    assert!(!out.to_string().contains("private prompt"));
    let accepted = wrap(&b, json!({"state":"native_accepted","native_queue_id":"q"}));
    assert_eq!(accepted["facts"]["accepted_native"], true);
    assert_eq!(accepted["facts"]["reply_observed"], false);
    assert_eq!(accepted["session_id"], b.request.session_id);
    assert_ne!(accepted["session_id"], accepted["provider_conversation_id"]);
}
