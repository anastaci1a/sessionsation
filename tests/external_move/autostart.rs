use super::*;
fn limits(used: f64, reset: i64) -> Value {
    json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":used,"windowDurationMins":300,"resetsAt":reset},
        "secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":daemon::now()+604800}}})
}
async fn setup(f: &Fixture) {
    let mut config = f.invoke(&["config", "get"]);
    config["near_limit_poll_seconds"] = json!(1);
    config["active_poll_seconds"] = json!(1);
    config["idle_poll_seconds"] = json!(1);
    daemon::call(&f.paths, "config/set", config).await.unwrap();
    f.invoke(&[
        "auto-start",
        "--thread",
        ID,
        "--enable",
        "--accept-experimental",
    ]);
}
fn fail_quota(f: &Fixture) {
    let mut s = f.a.lock().unwrap();
    let t = s.threads.get_mut(ID).unwrap();
    t["status"]["type"] = json!("systemError");
    t["turns"] = json!([{"id":"quota-turn","status":"failed","error":{"codexErrorInfo":"usageLimitExceeded"}}]);
    s.limits = Some(limits(100., daemon::now() + 1));
}
async fn policy_where(f: &Fixture, predicate: impl Fn(&Value) -> bool) -> Value {
    let until = Instant::now() + Duration::from_secs(18);
    loop {
        let value = f.invoke(&["auto-start", "--thread", ID]);
        if predicate(&value["policy"]) {
            return value["policy"].clone();
        }
        assert!(Instant::now() < until, "auto-start state: {value}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
fn starts(f: &Fixture) -> usize {
    f.a.lock()
        .unwrap()
        .calls
        .iter()
        .filter(|(m, _)| m == "turn/start")
        .count()
}
fn reset(f: &Fixture) {
    f.a.lock().unwrap().limits = Some(limits(2., daemon::now() + 18000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_failure_waits_for_fresh_reset_then_continues_once() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    paginated(&f, ID);
    fail_quota(&f);
    setup(&f).await;
    let pid = tmux(
        &f.socket,
        &["display-message", "-p", "-t", &pane, "#{pane_pid}"],
    );
    policy_where(&f, |p| p["waiting"].is_object()).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 0, "expired reset timestamp is not capacity");
    reset(&f);
    policy_where(&f, |p| {
        p["detail"]
            .as_str()
            .unwrap_or("")
            .contains("accepted the continuation")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 1);
    assert_eq!(
        pid,
        tmux(
            &f.socket,
            &["display-message", "-p", "-t", &pane, "#{pane_pid}"]
        )
    );
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "turn/steer" || m == "thread/archive")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_work_blocks_restart_without_being_stopped() {
    let f = Fixture::new().await;
    f.live(ID, false);
    fail_quota(&f);
    setup(&f).await;
    policy_where(&f, |p| p["waiting"].is_object()).await;
    f.a.lock().unwrap().background = true;
    reset(&f);
    policy_where(&f, |p| {
        p["detail"]
            .as_str()
            .unwrap_or("")
            .contains("background terminals")
    })
    .await;
    assert_eq!(starts(&f), 0);
    assert!(
        !f.a.lock()
            .unwrap()
            .calls
            .iter()
            .any(|(m, _)| m == "thread/backgroundTerminals/terminate")
    );
    f.a.lock().unwrap().background = false;
    policy_where(&f, |p| {
        p["detail"]
            .as_str()
            .unwrap_or("")
            .contains("accepted the continuation")
    })
    .await;
    assert_eq!(starts(&f), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_user_input_cancels_waiting_even_if_later_removed() {
    let f = Fixture::new().await;
    f.live(ID, false);
    fail_quota(&f);
    setup(&f).await;
    policy_where(&f, |p| p["waiting"].is_object()).await;
    f.a.lock().unwrap().queue_after_reads = Some(0);
    policy_where(&f, |p| p["waiting"].is_null()).await;
    f.a.lock().unwrap().queue_after_reads = None;
    reset(&f);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_before_exhaustion_and_other_errors_never_restart() {
    let f = Fixture::new().await;
    f.live(ID, false);
    setup(&f).await;
    f.a.lock().unwrap().limits = Some(limits(100., daemon::now() + 1));
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(f.invoke(&["auto-start", "--thread", ID])["policy"]["waiting"].is_null());
    {
        let mut s = f.a.lock().unwrap();
        let t = s.threads.get_mut(ID).unwrap();
        t["status"]["type"] = json!("systemError");
        t["turns"] =
            json!([{"id":"other-error","status":"failed","error":{"codexErrorInfo":"other"}}]);
    }
    tokio::time::sleep(Duration::from_secs(6)).await;
    reset(&f);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 0);
    assert!(f.invoke(&["auto-start", "--thread", ID])["policy"]["waiting"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabling_waiting_auto_start_prevents_continuation() {
    let f = Fixture::new().await;
    f.live(ID, false);
    fail_quota(&f);
    setup(&f).await;
    policy_where(&f, |p| p["waiting"].is_object()).await;
    f.invoke(&["auto-start", "--thread", ID, "--disable"]);
    reset(&f);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 0);
    assert_eq!(f.invoke(&["auto-start", "--thread", ID])["active"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_completed_turn_cancels_pending_restart() {
    let f = Fixture::new().await;
    f.live(ID, false);
    fail_quota(&f);
    setup(&f).await;
    policy_where(&f, |p| p["waiting"].is_object()).await;
    {
        let mut s = f.a.lock().unwrap();
        let t = s.threads.get_mut(ID).unwrap();
        t["status"]["type"] = json!("idle");
        t["turns"] = json!([{"id":"user-finished","status":"completed"}]);
    }
    reset(&f);
    policy_where(&f, |p| p["waiting"].is_null()).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_continuation_reply_disables_without_replay() {
    let f = Fixture::new().await;
    f.live(ID, false);
    fail_quota(&f);
    setup(&f).await;
    policy_where(&f, |p| p["waiting"].is_object()).await;
    f.a.lock().unwrap().lose_continuation_reply = true;
    reset(&f);
    policy_where(&f, |p| p["enabled"] == false).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn threshold_pause_restarts_only_the_work_that_received_a_stop() {
    let f = Fixture::new().await;
    f.live(ID, true);
    setup(&f).await;
    // Keep the stop evidence valid through the move worker's positive checks.
    f.a.lock().unwrap().limits = Some(limits(95., daemon::now() + 8));
    f.b.lock().unwrap().limits = Some(limits(95., daemon::now() + 8));
    for a in ["personal", "work"] {
        f.invoke(&["thresholds", a, "--multiplier", "1"]);
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    f.invoke(&[
        "auto-switch",
        "--thread",
        ID,
        "--accounts",
        "personal,work",
        "--enable",
        "--accept-experimental",
    ]);
    let job = f.automatic_job().await;
    f.wait(job["id"].as_str().unwrap(), "complete").await;
    let waiting = policy_where(&f, |p| p["waiting"].is_object()).await;
    assert_eq!(waiting["waiting"]["threshold_pause"], true);
    reset(&f);
    policy_where(&f, |p| {
        p["detail"]
            .as_str()
            .unwrap_or("")
            .contains("accepted the continuation")
    })
    .await;
    assert_eq!(starts(&f), 1);
    assert_eq!(f.invoke(&["auto-switch", "--thread", ID])["active"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deliberate_pause_disables_auto_start_before_quota_observation() {
    let f = Fixture::new().await;
    let pane = f.live(ID, false);
    setup(&f).await;
    // No pending restart yet: the explicit stop must still disable the option.
    let job = f.invoke(&["pause-session", "--pane", &pane]);
    f.wait(job["id"].as_str().unwrap(), "complete").await;
    assert_eq!(f.invoke(&["auto-start", "--thread", ID])["active"], false);
    fail_quota(&f);
    tokio::time::sleep(Duration::from_secs(6)).await;
    reset(&f);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(starts(&f), 0);
}
