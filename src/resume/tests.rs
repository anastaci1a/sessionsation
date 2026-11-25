use super::*;
fn receipt_for(paths: &Paths, session: &str, conversation: &str, phase: &str, age: i64) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let now = daemon::now();
    let r: Receipt = serde_json::from_value(json!({
        "version":1,
        "request":{"version":1,"request_id":id,"session_id":session,"expected_generation":"g",
            "account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},
            "space":{"kind":"existing","id":"w1"}},
        "directory":"/tmp","previous_name":"Name","conversation_id":conversation,"generation":"g",
        "phase":phase,"detail":"RESUME_ACK_UNCERTAIN","workspace_id":null,"tab_id":null,
        "pane_id":null,"operation_id":null,"created_at":now - age,"updated_at":now - age
    }))
    .unwrap();
    private_dir(&paths.root.join(crate::layout::RESUMPTIONS)).unwrap();
    fs::write(file(paths, &id).unwrap(), serde_json::to_vec(&r).unwrap()).unwrap();
    id
}
#[tokio::test]
async fn abandon_releases_uncertain_reservation_without_sending_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let id = receipt_for(&p, &session, "conv-1", "uncertain", 0);
    assert!(
        crate::lifecycle::pending(&p, "conv-1", None)
            .unwrap()
            .is_some()
    );
    let e = |r: Result<Value>| r.unwrap_err().to_string();
    assert_eq!(e(abandon(&p, &id, "other").await), "CONFIRMATION_MISMATCH");
    let done = abandon(&p, &id, &id).await.unwrap();
    assert_eq!(done["phase"], "abandoned");
    assert_eq!(done["abandoned_from"], "uncertain");
    assert!(
        done["detail"]
            .as_str()
            .unwrap()
            .contains("nothing was sent")
    );
    assert!(
        crate::lifecycle::pending(&p, "conv-1", None)
            .unwrap()
            .is_none()
    );
    // Terminal and never revived by status().
    assert_eq!(status(&p, &id).await.unwrap().phase, "abandoned");
    assert_eq!(e(abandon(&p, &id, &id).await), "OPERATION_ALREADY_TERMINAL");
}
#[tokio::test]
async fn abandon_refuses_fresh_in_flight_operations_and_active_workers() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let fresh = receipt_for(&p, &session, "conv-2", "dispatching", 0);
    assert_eq!(
        abandon(&p, &fresh, &fresh).await.unwrap_err().to_string(),
        "OPERATION_NOT_STUCK"
    );
    let stuck = receipt_for(&p, &session, "conv-2", "dispatching", 60);
    let lock = lock_file(&file(&p, &stuck).unwrap().with_extension("worker.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    assert_eq!(
        abandon(&p, &stuck, &stuck).await.unwrap_err().to_string(),
        "OPERATION_WORKER_ACTIVE"
    );
    drop(lock);
    assert_eq!(
        abandon(&p, &stuck, &stuck).await.unwrap()["abandoned_from"],
        "dispatching"
    );
}
#[tokio::test]
async fn abandon_refuses_while_the_session_process_is_live() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let start: u64 = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let live = catalog::record_launch(
        &p,
        "fixture",
        "conv-3",
        "live",
        Some(catalog::Attachment {
            host: "h".into(),
            target: "t".into(),
            pid: std::process::id(),
            process_start: start,
        }),
    )
    .unwrap();
    let id = receipt_for(&p, &live.id, "conv-3", "uncertain", 0);
    assert_eq!(
        abandon(&p, &id, &id).await.unwrap_err().to_string(),
        "SESSION_PROCESS_LIVE"
    );
    assert_eq!(read(&p, &id).unwrap().phase, "uncertain");
    // A different recorded process start time means that process is gone.
    catalog::record_launch(
        &p,
        "fixture",
        "conv-3",
        "live",
        Some(catalog::Attachment {
            host: "h".into(),
            target: "t".into(),
            pid: std::process::id(),
            process_start: start + 1,
        }),
    )
    .unwrap();
    // With the attachment gone the live scan still runs and finds no provider.
    let r = abandon(&p, &id, &id).await;
    assert!(
        r.is_ok()
            || r.unwrap_err()
                .to_string()
                .contains("SESSION_STATE_UNVERIFIED")
    );
}
#[test]
fn unfinished_consent_never_bypasses_other_history_gates() {
    let mut source: external::Instance = serde_json::from_value(json!({
        "inactive":true,"pane":"","tmux_session":"","pid":0,"cwd":"/tmp",
        "thread_id":"fixture","rollout":"/tmp/history","home":"/tmp/home",
        "binary":"codex","resume_options":[],"block_reason":"unfinished"
    }))
    .unwrap();
    assert!(
        validate_saved_history(&source, false)
            .unwrap_err()
            .to_string()
            .contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED")
    );
    assert!(validate_saved_history(&source, true).is_ok());
    for reason in ["busy", "unknown", "diverged", "moving", "future"] {
        source.block_reason = Some(reason.into());
        assert!(validate_saved_history(&source, true).is_err());
    }
    source.block_reason = Some("unfinished".into());
    source.move_id = Some("operation".into());
    assert!(validate_saved_history(&source, true).is_err());
    source.move_id = None;
    source.inactive = false;
    assert!(validate_saved_history(&source, true).is_err());
}
#[test]
fn directory_override_preserves_writer_and_unfinished_history_guards() {
    let tmp = tempfile::tempdir().unwrap();
    let rollout = tmp.path().join("history.jsonl");
    fs::write(
        &rollout,
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
    )
    .unwrap();
    let source: external::Instance = serde_json::from_value(json!({"inactive":true,"pane":"","tmux_session":"","pid":0,"cwd":tmp.path().join("missing"),"thread_id":"fixture","rollout":rollout,"home":tmp.path(),"binary":"codex","resume_options":[],"block_reason":"unknown"})).unwrap();
    let mut r: Request = serde_json::from_value(json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"},"directory":{"path":tmp.path(),"create":[],"confirmed_existing":true}})).unwrap();
    assert!(validate_resume_history(&source, &r).is_ok());
    let writer = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
    assert!(validate_resume_history(&source, &r).is_err());
    drop(writer);
    fs::write(
        &rollout,
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
    )
    .unwrap();
    assert!(
        validate_resume_history(&source, &r)
            .unwrap_err()
            .to_string()
            .contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED")
    );
    r.confirm_unfinished = true;
    assert!(validate_resume_history(&source, &r).is_ok());
    r.directory = None;
    assert!(validate_resume_history(&source, &r).is_err());
}
#[test]
fn legacy_resume_receipt_preserves_saved_directory_and_request_identity() {
    let request = json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"}});
    let receipt: Receipt = serde_json::from_value(json!({"version":1,"request":request,"directory":"/original","previous_name":"Old","conversation_id":"thread","generation":"g","phase":"running","updated_at":1})).unwrap();
    assert_eq!(saved_directory(&receipt), std::path::Path::new("/original"));
    assert!(receipt.created_directories.is_empty());
    // Contract 1.52.0: pane_reused defaults to false and is omitted while false.
    assert!(!receipt.pane_reused);
    let mut stored = serde_json::to_value(&receipt).unwrap();
    assert!(stored.get("pane_reused").is_none());
    let mut reused = receipt.clone();
    reused.pane_reused = true;
    assert_eq!(serde_json::to_value(&reused).unwrap()["pane_reused"], true);
    stored["pane_reused"] = json!(true);
    assert!(
        serde_json::from_value::<Receipt>(stored)
            .unwrap()
            .pane_reused
    );
    assert_eq!(
        serde_json::to_value(&receipt.request).unwrap()["directory"],
        Value::Null
    );
}
#[test]
fn request_directory_is_explicit_and_unknown_provider_is_rejected() {
    let mut value = json!({"version":1,"request_id":uuid::Uuid::new_v4(),"session_id":uuid::Uuid::new_v4(),"expected_generation":"g","account_id":"codex:fixture","name":"Name","host":{"session":"fixture","socket":"/tmp/socket"},"space":{"kind":"existing","id":"w1"}});
    assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
    for name in ["x".repeat(64), "界".repeat(64)] {
        value["name"] = json!(name);
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
    }
    for name in ["x".repeat(65), "界".repeat(65)] {
        value["name"] = json!(name);
        assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_err());
    }
    value["name"] = json!("Name");
    value["directory"] = json!("/different");
    assert!(serde_json::from_value::<Request>(value.clone()).is_err());
    value["directory"] = json!({"path":"/different","create":[],"confirmed_existing":true});
    assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
    value.as_object_mut().unwrap().remove("directory");
    // Contract 1.66.0: Claude resumes natively; Gemini is refused.
    value["account_id"] = json!("claude:fixture");
    assert!(shape(&serde_json::from_value(value.clone()).unwrap()).is_ok());
    value["account_id"] = json!("gemini:fixture");
    assert!(shape(&serde_json::from_value(value).unwrap()).is_err());
}
