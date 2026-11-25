use super::super::*;

fn job(kind: &str, phase: &str, created_at: i64, thread: &str) -> Job {
    serde_json::from_value(json!({
        "id": "stop-1", "kind": kind, "target": "", "phase": phase, "detail": "",
        "created_at": created_at,
        "source": {"account":"personal","pane":"%1","tmux_session":"fixture","pid":1,
            "cwd":"/fixture","thread_id":thread,"rollout":"/fixture/rollout",
            "home":"/fixture","binary":"fake","resume_options":[]}
    }))
    .unwrap()
}

#[test]
fn a_cancelled_or_failed_stop_job_does_not_leave_a_stale_gate_on_a_live_session() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    let thread = uuid::Uuid::new_v4().to_string();
    let me = std::process::id();
    let start = process_info(me).unwrap().start_time;
    let gate = |phase: &str, pid: u32, start_time: u64, thread_id: &str| {
        let mut j = job("terminate", phase, daemon::now(), thread_id);
        j.id = uuid::Uuid::new_v4().to_string();
        j.source.pid = pid;
        j.source.start_time = start_time;
        save_job(&paths, &j).unwrap();
        super::super::catalog::set_stop_intent(&paths, &thread, Some(j.id.clone())).unwrap();
        j.id
    };
    let stop = |id: &str| {
        super::super::messaging_stop_intent(&paths, &thread)
            .unwrap()
            .as_deref()
            == Some(id)
    };
    // A finished stop keeps its gate: the session really was stopped.
    let done = gate("complete", me, start, &thread);
    assert!(stop(&done));
    // Cancelled, but the process it was going to stop is still the same one: stale.
    gate("cancelled", me, start, &thread);
    assert!(
        super::super::messaging_stop_intent(&paths, &thread)
            .unwrap()
            .is_none()
    );
    assert!(
        super::super::catalog::stop_intent(&paths, &thread)
            .unwrap()
            .is_none(),
        "healed in the catalog"
    );
    gate("failed", me, start, &thread);
    assert!(
        super::super::messaging_stop_intent(&paths, &thread)
            .unwrap()
            .is_none()
    );
    // The process is gone or was replaced (other start time): the gate stands.
    let replaced = gate("cancelled", me, start + 1, &thread);
    assert!(stop(&replaced));
    // A job that names another conversation never heals this one.
    let other = gate("cancelled", me, start, &uuid::Uuid::new_v4().to_string());
    assert!(stop(&other));
}

#[tokio::test]
async fn a_stop_waiting_for_idle_expires_and_releases_its_stop_gate() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    let thread = uuid::Uuid::new_v4().to_string();
    let now = daemon::now();
    // A stop whose client was interrupted: the worker still waits for idle.
    let old = job(
        "terminate",
        "waiting_for_idle",
        now - TERMINATE_WAIT_SECONDS - 5,
        &thread,
    );
    assert_eq!(old.expires_at(), Some(now - 5));
    super::super::catalog::set_stop_intent(&paths, &thread, Some("stop-1".into())).unwrap();
    let error = tick(&paths, &old).await.unwrap_err().to_string();
    assert!(error.contains("expired") && error.contains("nothing was stopped"));
    // Its stop gate went with it, so sends are not refused for a stop that never ran.
    assert!(
        super::super::catalog::stop_intent(&paths, &thread)
            .unwrap()
            .is_none()
    );

    // A newer stop's gate is never released by an older request's expiry.
    super::super::catalog::set_stop_intent(&paths, &thread, Some("stop-2".into())).unwrap();
    assert!(tick(&paths, &old).await.is_err());
    assert_eq!(
        super::super::catalog::stop_intent(&paths, &thread)
            .unwrap()
            .as_deref(),
        Some("stop-2")
    );

    // Within the window, other kinds and non-waiting phases keep going.
    for waiting in [
        job("terminate", "waiting_for_idle", now, &thread),
        job(
            "account",
            "waiting_for_idle",
            now - 10 * TERMINATE_WAIT_SECONDS,
            &thread,
        ),
        job(
            "terminate",
            "stopping_source",
            now - 10 * TERMINATE_WAIT_SECONDS,
            &thread,
        ),
    ] {
        tick(&paths, &waiting).await.unwrap();
    }
    // A journal that predates `expires_at` gets the default window from its creation.
    let mut explicit = job("terminate", "waiting_for_idle", now, &thread);
    explicit.expires_at = now + 7;
    assert_eq!(explicit.expires_at(), Some(now + 7));
}
