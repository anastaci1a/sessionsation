use super::super::*;

#[test]
fn pending_stops_are_listed_with_their_expiry_per_session() {
    let job = |id: &str, kind: &str, phase: &str, thread: &str| -> external::Job {
        serde_json::from_value(json!({"id":id,"kind":kind,"target":"","phase":phase,"detail":"",
            "created_at":100,"source":{"account":"a","pane":"%1","tmux_session":"t","pid":1,
            "cwd":"/","thread_id":thread,"rollout":"/r","home":"/h","binary":"b","resume_options":[]}}))
        .unwrap()
    };
    let jobs = vec![
        job("s", "terminate", "waiting_for_idle", "one"),
        job("m", "account", "queued", "one"),
        job("done", "terminate", "complete", "one"),
        job("other", "terminate", "waiting_for_idle", "two"),
    ];
    let listed = pending_operations(&jobs, "one");
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["kind"], "stop");
    assert_eq!(listed[0]["phase"], "waiting_for_idle");
    assert_eq!(
        listed[0]["expires_at"],
        100 + external::TERMINATE_WAIT_SECONDS
    );
    assert_eq!(listed[1]["kind"], "switch");
    assert!(listed[1]["expires_at"].is_null());
    assert_eq!(pending_operations(&jobs, "none"), json!([]));
}
