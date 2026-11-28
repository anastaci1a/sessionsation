use super::super::*;

fn queued(paths: &Paths, phase_name: &str, detail: &str) -> Value {
    let session =
        crate::catalog::record_launch(paths, "acct", &uuid::Uuid::new_v4().to_string(), "s", None)
            .unwrap();
    let o = Operation {
        id: uuid::Uuid::new_v4().to_string(),
        kind: Kind::Archive,
        session,
        source: None,
        expected_generation: String::new(),
        phase: phase_name.into(),
        detail: detail.into(),
        created_at: daemon::now(),
        updated_at: daemon::now(),
        worker_pid: None,
        worker_start: None,
        nothing_to_archive: false,
        respawned_at: None,
        detached_from: None,
    };
    op_save(paths, &o).unwrap();
    o.public()
}

#[tokio::test]
async fn wait_reports_failure_success_and_timeout() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let bound = std::time::Duration::from_millis(300);
    let failed = queued(
        &paths,
        "failed",
        "ACCOUNT_BUSY: held (os error 11). No automatic retry",
    );
    let e = wait_within(&paths, failed, bound).await.unwrap_err();
    let n = e
        .downcast_ref::<crate::session_model::NotConfirmed>()
        .unwrap();
    assert_eq!(n.code, "OPERATION_FAILED");
    assert!(n.message.starts_with("ACCOUNT_BUSY"));
    assert_eq!(n.operation["phase"], "failed");
    let done = queued(&paths, "complete", "ok");
    let v = wait_within(&paths, done, bound).await.unwrap();
    assert_eq!(v["phase"], "complete");
    let stuck = queued(&paths, "waiting_for_account", WAITING_DETAIL);
    let e = wait_within(&paths, stuck, bound).await.unwrap_err();
    assert_eq!(
        e.downcast_ref::<crate::session_model::NotConfirmed>()
            .unwrap()
            .code,
        "OPERATION_TIMEOUT"
    );
}
