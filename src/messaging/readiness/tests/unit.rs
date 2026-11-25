use super::super::*;
fn session() -> catalog::Session {
    catalog::Session {
        id: uuid::Uuid::new_v4().to_string(),
        name: "test".into(),
        provider: "codex".into(),
        account: "selected".into(),
        conversation_id: uuid::Uuid::new_v4().to_string(),
        revision: 1,
        attachment: None,
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    }
}
#[test]
fn passive_hints_never_adopt_legacy_busy_steer_or_infer_stop_intent() {
    let s = session();
    for state in [
        "idle",
        "working",
        "interrupted",
        "errored",
        "disconnected",
        "unknown",
    ] {
        let native = json!({"state":state,"observed_at":100,"valid_until":105,"native_sending_enabled":true,"actions":{"submit":{"effective_action":"steer"}},"private":"SECRET"});
        let out = project(&s, native, false, 101);
        assert_eq!(
            out["actions"]["submit"]["available"],
            matches!(state, "idle" | "errored")
        );
        assert_eq!(
            out["actions"]["queue"]["available"],
            matches!(state, "idle" | "working" | "errored")
        );
        assert_eq!(out["explicit_intent"]["state"], "unknown");
        assert_eq!(
            out["actions"]["resume"]["requires_launch_plan"],
            state == "disconnected"
        );
        assert_eq!(
            out["actions"]["resume"]["requires_expected_runtime_generation"],
            state != "disconnected"
        );
        assert!(!out.to_string().contains("SECRET"));
        assert!(out["actions"]["submit"]["effective_action"].is_null());
        assert_eq!(out["actions"]["steer"]["available"], state == "working");
    }
}
#[test]
fn stale_disabled_removed_and_explicit_stop_block_all_readiness() {
    for (removed, now, enabled, stop) in [
        (false, 105, true, None),
        (false, 99, true, None),
        (true, 101, true, None),
        (false, 101, false, None),
        (false, 101, true, Some("stop-token")),
    ] {
        let out = project(
            &session(),
            json!({"state":"idle","observed_at":100,"valid_until":105,"native_sending_enabled":enabled,"stop_intent_id":stop}),
            removed,
            now,
        );
        assert_eq!(out["actions"]["submit"]["available"], false);
        assert_eq!(out["actions"]["queue"]["available"], false);
        assert!(out["actions"]["queue"]["reason"].is_string());
    }
}
