//! Contract 1.73.3: a queued operation whose worker never started.
use super::super::*;
use std::cell::Cell;

fn stalled(paths: &Paths, age: i64) -> Operation {
    let session =
        crate::catalog::record_launch(paths, "acct", &uuid::Uuid::new_v4().to_string(), "s", None)
            .unwrap();
    let o = Operation {
        id: uuid::Uuid::new_v4().to_string(),
        kind: Kind::Archive,
        session,
        source: None,
        expected_generation: String::new(),
        phase: "queued".into(),
        detail: "Confirmed; waiting for conversation checks.".into(),
        created_at: daemon::now() - age,
        updated_at: daemon::now() - age,
        worker_pid: None,
        worker_start: None,
        nothing_to_archive: false,
        respawned_at: None,
    };
    op_save(paths, &o).unwrap();
    o
}

fn age(paths: &Paths, id: &str, seconds: i64) {
    let mut o = op_load(paths, id).unwrap();
    o.updated_at = daemon::now() - seconds;
    op_save(paths, &o).unwrap();
}

#[test]
fn a_workerless_queued_operation_is_started_once_then_failed() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let o = stalled(&paths, 30);
    let fresh = stalled(&paths, 0);
    let mut recorded = stalled(&paths, 30);
    recorded.worker_pid = Some(1);
    op_save(&paths, &recorded).unwrap();
    let calls = Cell::new(0);
    let count = |_: &Paths, _: &Operation| {
        calls.set(calls.get() + 1);
        Ok(())
    };
    start_stalled_with(&paths, count).unwrap();
    assert_eq!(
        calls.get(),
        1,
        "only the stalled record, not the fresh or recorded one"
    );
    let after = op_load(&paths, &o.id).unwrap();
    assert_eq!(after.phase, "queued");
    assert!(after.respawned_at.is_some());
    assert!(
        op_path(&paths, &o.id)
            .map(fs::read)
            .unwrap()
            .unwrap()
            .windows(12)
            .any(|w| w == b"respawned_at")
    );
    // Within the grace of the retry: left alone.
    start_stalled_with(&paths, count).unwrap();
    assert_eq!(calls.get(), 1);
    // The retried worker never recorded itself either: failed, no second start.
    age(&paths, &o.id, 30);
    start_stalled_with(&paths, count).unwrap();
    assert_eq!(calls.get(), 1);
    let failed = op_load(&paths, &o.id).unwrap();
    assert_eq!(failed.phase, "failed");
    assert_eq!(
        failed.detail,
        "Worker did not start after one automatic retry; nothing was changed."
    );
    assert_eq!(op_load(&paths, &fresh.id).unwrap().phase, "queued");
    assert_eq!(op_load(&paths, &recorded.id).unwrap().phase, "queued");
    // Old records (no field) still parse, and a record without a retry does not write it.
    assert!(
        !op_path(&paths, &fresh.id)
            .map(fs::read)
            .unwrap()
            .unwrap()
            .windows(12)
            .any(|w| w == b"respawned_at")
    );
}

#[test]
fn a_held_lock_is_left_alone() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let o = stalled(&paths, 30);
    let lock =
        crate::paths::lock_file(&op_path(&paths, &o.id).unwrap().with_extension("lock")).unwrap();
    lock.lock_exclusive().unwrap();
    start_stalled_with(&paths, |_, _| panic!("a worker holds the lock")).unwrap();
    // A failed spawn never writes over a record whose worker holds the lock.
    let _ = launch_with(&paths, &o, |_| anyhow::bail!("no executable"));
    let after = op_load(&paths, &o.id).unwrap();
    assert_eq!((after.phase.as_str(), after.respawned_at), ("queued", None));
}

#[test]
fn a_worker_that_cannot_start_leaves_the_record_failed() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let o = stalled(&paths, 0);
    let e = launch_with(&paths, &o, |_| anyhow::bail!("no executable")).unwrap_err();
    assert_eq!(e.to_string(), "no executable");
    let after = op_load(&paths, &o.id).unwrap();
    assert_eq!(after.phase, "failed");
    assert_eq!(after.detail, "Worker did not start: no executable");
    // The sweep's own retry failing to spawn fails the record the same way.
    let retried = stalled(&paths, 30);
    let _ = start_stalled_with(&paths, |p, o| launch_with(p, o, |_| anyhow::bail!("gone")));
    assert_eq!(op_load(&paths, &retried.id).unwrap().phase, "failed");
}

fn phase_of(paths: &Paths, id: &str) -> String {
    let all = operations(paths).unwrap();
    let o = all.iter().find(|o| o["id"] == id).unwrap();
    o["phase"].as_str().unwrap().to_owned()
}

/// Review of 1.73.3: neither `operations` nor `--wait` reports `failed` before the one retry,
/// so a client never sees failed and then a run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_retry_complete_never_reports_failed_first() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let o = stalled(&paths, 600);
    assert_eq!(
        phase_of(&paths, &o.id),
        "queued",
        "old and workerless, not yet retried"
    );
    let waiting = tokio::spawn({
        let (paths, queued) = (paths.clone(), o.public());
        async move { wait_within(&paths, queued, std::time::Duration::from_secs(10)).await }
    });
    let watcher = std::thread::spawn({
        let (paths, id) = (paths.clone(), o.id.clone());
        move || {
            let mut seen = vec![];
            while seen.last().is_none_or(|p| p != "complete") {
                seen.push(phase_of(&paths, &id));
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            seen
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    // The retried worker records itself under the lock, then completes.
    start_stalled_with(&paths, |paths, o| {
        let (paths, id) = (paths.clone(), o.id.clone());
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let (_lock, mut o) = locked(&paths, &id).unwrap();
            o.worker_pid = Some(std::process::id());
            o.worker_start = Some(process_info(std::process::id()).unwrap().start_time);
            op_save(&paths, &o).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            phase(&paths, &mut o, "complete", "done").unwrap();
        });
        Ok(())
    })
    .unwrap();
    let done = waiting.await.unwrap().unwrap();
    assert_eq!(done["phase"], "complete");
    let seen = watcher.join().unwrap();
    assert!(!seen.iter().any(|p| p == "failed"), "{seen:?}");
}

#[test]
fn the_projection_stays_queued_until_the_sweep_writes_failed() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let o = stalled(&paths, 30);
    start_stalled_with(&paths, |_, _| Ok(())).unwrap();
    assert_eq!(phase_of(&paths, &o.id), "queued");
    // Past the retry's grace, a slow retry worker may still take the lock: still queued.
    age(&paths, &o.id, 600);
    assert_eq!(phase_of(&paths, &o.id), "queued");
    start_stalled_with(&paths, |_, _| panic!("no second start")).unwrap();
    let shown = operations(&paths).unwrap();
    let shown = shown.iter().find(|v| v["id"] == o.id.as_str()).unwrap();
    assert_eq!(
        (shown["phase"].as_str(), shown["detail"].as_str()),
        (Some("failed"), Some(RETRY_FAILED))
    );
}

/// Coordinator decision for 1.73.3: a recorded worker that is gone is never replayed; the
/// operator abandons it, which frees the session.
#[test]
fn a_gone_recorded_worker_is_abandoned_by_the_operator() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut o = stalled(&paths, 30);
    o.phase = "checking_history".into();
    o.worker_pid = Some(std::process::id());
    o.worker_start = Some(process_info(std::process::id()).unwrap().start_time);
    op_save(&paths, &o).unwrap();
    let refused = |e: anyhow::Error| e.to_string();
    assert_eq!(
        refused(abandon(&paths, &o.id, "other").unwrap_err()),
        "CONFIRMATION_MISMATCH"
    );
    assert_eq!(
        refused(abandon(&paths, &o.id, &o.id).unwrap_err()),
        "OPERATION_WORKER_ACTIVE",
        "alive"
    );
    let thread = o.session.conversation_id.clone();
    let blocked = crate::lifecycle::ensure_available(&paths, &thread, None).unwrap_err();
    assert!(
        blocked
            .to_string()
            .contains("api abandon-operation it (operator) if its worker is gone"),
        "{blocked}"
    );
    // Same pid, another start time: that worker is gone.
    o.worker_start = Some(o.worker_start.unwrap() + 1);
    op_save(&paths, &o).unwrap();
    let lock =
        crate::paths::lock_file(&op_path(&paths, &o.id).unwrap().with_extension("lock")).unwrap();
    lock.lock_exclusive().unwrap();
    assert_eq!(
        refused(abandon(&paths, &o.id, &o.id).unwrap_err()),
        "OPERATION_WORKER_ACTIVE",
        "lock held"
    );
    drop(lock);
    let v = abandon(&paths, &o.id, &o.id).unwrap();
    assert_eq!(v["phase"], "failed");
    assert!(
        v["detail"]
            .as_str()
            .unwrap()
            .starts_with("Abandoned by the operator: worker gone")
    );
    crate::lifecycle::ensure_available(&paths, &thread, None).unwrap();
    assert_eq!(
        refused(abandon(&paths, &o.id, &o.id).unwrap_err()),
        "OPERATION_ALREADY_TERMINAL"
    );
    // A never-started record is the sweep's, not the operator's.
    let workerless = stalled(&paths, 30);
    assert_eq!(
        refused(abandon(&paths, &workerless.id, &workerless.id).unwrap_err()),
        "OPERATION_NOT_STUCK"
    );
}
