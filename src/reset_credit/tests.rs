use super::*;

#[test]
fn summary_and_outcomes_parse() {
    let s = parse_summary(
        &json!({"availableCount":2,"credits":[{"id":"c1","status":"available","expiresAt":99,"grantedAt":1,"resetType":"codexRateLimits"},{"bad":1}]}),
        7,
    )
    .unwrap();
    assert_eq!(s.available, 2);
    assert_eq!(s.observed_at, 7);
    assert_eq!(
        s.credits.unwrap(),
        vec![ResetCredit {
            id: "c1".into(),
            status: Some("available".into()),
            expires_at: Some(99),
            ..Default::default()
        }]
    );
    assert!(
        parse_summary(&json!({"availableCount":0}), 1)
            .unwrap()
            .credits
            .is_none()
    );
    assert!(parse_summary(&Value::Null, 1).is_none());
    for o in OUTCOMES {
        assert_eq!(parse_outcome(&json!({ "outcome": o })).unwrap(), *o);
    }
    assert!(parse_outcome(&json!({"outcome":"maybe"})).is_err());
    assert!(validate_request_id("not-a-uuid").is_err());
    assert!(validate_request_id(&uuid::Uuid::new_v4().to_string()).is_ok());
}

#[test]
fn retry_reuses_the_record_and_key_and_refuses_another_target() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut r = admit(&paths, ProviderId::Codex, "codex:a", None, &id, "v").unwrap();
    assert_eq!((r.phase.as_str(), r.attempts), ("requesting", 1));
    assert_eq!(Record::read(&paths, &id).unwrap().idempotency_key, id);
    finish(&paths, &mut r, "uncertain", None, "x").unwrap();
    let again = admit(&paths, ProviderId::Codex, "codex:a", None, &id, "v").unwrap();
    assert_eq!((again.phase.as_str(), again.attempts), ("requesting", 2));
    assert_eq!(again.idempotency_key, id);
    assert_eq!(again.created_at, r.created_at);
    for (account, credit) in [("codex:b", None), ("codex:a", Some("c1"))] {
        let e = admit(&paths, ProviderId::Codex, account, credit, &id, "v").unwrap_err();
        assert!(e.to_string().starts_with("REQUEST_ID_CONFLICT"), "{e}");
    }
    assert_eq!(Record::list(&paths).unwrap().len(), 1);
}

#[test]
fn failures_are_classified_without_the_provider_message() {
    let unknown = anyhow::Error::new(crate::rpc::Rejected::new(
        "app-server rejected request (code -32600)".into(),
        &json!({"code":-32600,"message":"Invalid request: unknown variant `account/rateLimitResetCredit/consume`"}),
    ));
    let (phase, msg) = failure(&unknown, "codex-cli 0.1.0", "r");
    assert_eq!(phase, "failed");
    assert!(msg.starts_with("UNSUPPORTED_PROVIDER_OPERATION: codex codex-cli 0.1.0"));
    let missing = anyhow::Error::new(crate::rpc::Rejected::new(
        "x".into(),
        &json!({"code":-32601,"message":"Method not found"}),
    ));
    assert!(failure(&missing, "v", "r").1.starts_with("UNSUPPORTED_"));
    let backend = anyhow::Error::new(crate::rpc::Rejected::new(
        "app-server rejected request (code -32603)".into(),
        &json!({"code":-32603,"message":"secret text"}),
    ));
    let (phase, msg) = failure(&backend, "v", "r");
    assert_eq!(phase, "failed");
    assert!(msg.starts_with("RESET_CREDIT_FAILED") && !msg.contains("secret"));
    let (phase, msg) = failure(&anyhow::anyhow!("timed out"), "v", "r");
    assert_eq!(phase, "uncertain");
    assert!(msg.starts_with("RESET_CREDIT_OUTCOME_UNKNOWN") && msg.contains("--request-id r"));
}
