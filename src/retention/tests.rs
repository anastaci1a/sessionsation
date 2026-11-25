use super::*;
use std::time::Duration;

fn aged(path: &Path, days: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(days * DAY + 60))
        .unwrap();
}

#[test]
fn retention_removes_only_old_final_history_and_rotates_logs() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    layout::ensure_dirs(root).unwrap();
    let policy = Policy {
        log_days: 30,
        log_max_bytes: 10,
        cache_days: 14,
        operation_days: 90,
    };
    // logs
    fs::write(root.join(layout::DAEMON_LOG), "0123456789abcdef").unwrap();
    fs::write(root.join("logs/handoff.log.1"), "old").unwrap();
    aged(&root.join("logs/handoff.log.1"), 31);
    // cache
    let obs = root.join(layout::OBSERVATIONS);
    fs::create_dir_all(&obs).unwrap();
    for (name, days) in [("old.json", 15), ("new.json", 1)] {
        fs::write(obs.join(name), "{}").unwrap();
        aged(&obs.join(name), days);
    }
    let snapshot = layout::state_snapshot(root, "active");
    fs::write(&snapshot, "{}").unwrap();
    aged(&snapshot, 400);
    // operations
    let handoff = root.join(layout::HANDOFF_JOBS);
    fs::create_dir_all(&handoff).unwrap();
    for (id, phase, days) in [
        ("done", "complete", 91),
        ("recent", "complete", 10),
        ("unsure", "uncertain", 400),
        ("busy", "failed", 400),
    ] {
        fs::write(
            handoff.join(format!("{id}.json")),
            json!({"phase":phase}).to_string(),
        )
        .unwrap();
        aged(&handoff.join(format!("{id}.json")), days);
        fs::write(handoff.join(format!("{id}.worker-lock")), "").unwrap();
    }
    let busy = fs::File::open(handoff.join("busy.worker-lock")).unwrap();
    busy.lock_exclusive().unwrap();
    let deletions = root.join(layout::DELETIONS);
    fs::create_dir_all(&deletions).unwrap();
    fs::write(
        deletions.join("x.json"),
        json!({"phase":"complete"}).to_string(),
    )
    .unwrap();
    aged(&deletions.join("x.json"), 1000);

    let dry = sweep(root, policy, SystemTime::now(), true).unwrap();
    assert_eq!(dry["operations_removed"][layout::HANDOFF_JOBS], 1);
    assert!(
        handoff.join("done.json").exists(),
        "dry run changes nothing"
    );

    let r = sweep(root, policy, SystemTime::now(), false).unwrap();
    assert_eq!(r["cache_removed"], 1);
    assert_eq!(r["operations_removed"][layout::HANDOFF_JOBS], 1);
    assert!(!handoff.join("done.json").exists() && !handoff.join("done.worker-lock").exists());
    for kept in [
        "recent.json",
        "unsure.json",
        "busy.json",
        "busy.worker-lock",
    ] {
        assert!(handoff.join(kept).exists(), "{kept}");
    }
    assert!(
        deletions.join("x.json").exists(),
        "tombstones are never removed"
    );
    assert!(snapshot.exists() && obs.join("new.json").exists() && !obs.join("old.json").exists());
    assert_eq!(fs::read(root.join(layout::DAEMON_LOG)).unwrap().len(), 0);
    assert_eq!(
        fs::read_to_string(root.join("logs/daemon.log.1")).unwrap(),
        "0123456789abcdef"
    );
    assert!(!root.join("logs/handoff.log.1").exists());
    drop(busy);
}

#[test]
fn an_invalid_config_is_reported_and_the_defaults_used() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let ok = run(&paths, true).unwrap();
    assert_eq!(ok["config_error"], Value::Null);
    assert_eq!(ok["policy"]["receipt_unknown_horizon_days"], 90);
    // A horizon shorter than the retention fails validation.
    fs::write(
        paths.root.join("config.json"),
        r#"{"receipt_retention_days":30,"receipt_unknown_horizon_days":7}"#,
    )
    .unwrap();
    let bad = run(&paths, true).unwrap();
    assert!(
        bad["config_error"]
            .as_str()
            .is_some_and(|e| e.contains("receipt_unknown_horizon_days")),
        "{bad}"
    );
    let d = crate::config::Config::default();
    assert_eq!(
        bad["policy"]["receipt_retention_days"],
        d.receipt_retention_days
    );
    assert_eq!(
        bad["policy"]["receipt_unknown_horizon_days"],
        d.receipt_unknown_horizon_days
    );
    assert_eq!(bad["policy"]["log_retention_days"], d.log_retention_days);
}
