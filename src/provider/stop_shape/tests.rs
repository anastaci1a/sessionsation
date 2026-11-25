use super::*;
#[test]
fn every_result_gets_the_shared_keys_and_keeps_its_own() {
    let s = crate::catalog::Session {
        id: "s1".into(),
        name: "n".into(),
        provider: "claude".into(),
        account: "a".into(),
        conversation_id: "c1".into(),
        revision: 1,
        attachment: None,
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
    let already = shape(
        json!({"session_id":"s1","already_stopped":true,"phase":"complete"}),
        &s,
    );
    let done = shape(
        json!({"id":"x","source_session_id":"s1","kind":"terminate","phase":"complete","pid":7,"signal":"SIGTERM","extra":1}),
        &s,
    );
    for v in [&already, &done] {
        for k in KEYS {
            assert!(v.get(k).is_some(), "{k} missing in {v}");
        }
        assert_eq!(v["session_id"], "s1");
        assert_eq!(v["source_session_id"], "s1");
        assert_eq!(v["provider"], "claude");
        assert_eq!(v["source_account_id"], "claude:a");
    }
    assert_eq!(already["already_stopped"], true);
    assert_eq!(already["interrupted_turn"], false);
    assert!(already["pid"].is_null());
    assert_eq!(done["already_stopped"], false);
    assert_eq!(done["pid"], 7);
    assert_eq!(done["extra"], 1);
    assert!(done["auto_start"].is_null());
}
