use super::*;
fn setup() -> (tempfile::TempDir, Paths, Session, Host) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(temp.path().join("data"))).unwrap();
    let session = record_launch(
        &paths,
        "personal",
        "conversation",
        "legacy",
        Some(Attachment {
            host: "herdr".into(),
            target: "herdr/default/w1:p1".into(),
            pid: 42,
            process_start: 77,
        }),
    )
    .unwrap();
    let host = Host {
        session: "default".into(),
        socket: "/private/host-one.sock".into(),
    };
    (temp, paths, session, host)
}
fn snapshot(space: &str, label: &str) -> (i64, Value) {
    (
        daemon::now(),
        json!({"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":space}],"tabs":[{"tab_id":"w1:t1","workspace_id":space}],"workspaces":[{"workspace_id":space,"label":label}]}),
    )
}
fn get(paths: &Paths, s: &Session) -> Value {
    project(paths, std::slice::from_ref(s)).unwrap()["memberships"][&s.id].clone()
}
#[test]
fn legacy_live_without_receipts_and_private_locators() {
    let (_t, p, s, h) = setup();
    record(&p, vec![(s.clone(), h, Some(snapshot("w1", "Work")))]).unwrap();
    let v = get(&p, &s);
    assert_eq!(v["state"], "observed");
    assert_eq!(v["space"]["label"], "Work");
    let all = project(&p, &[s]).unwrap().to_string();
    assert!(!all.contains("/private"));
    assert!(!all.contains("socket"));
    assert!(!all.contains("w1:p1"));
}
#[test]
fn host_scoped_duplicate_names_and_native_ids() {
    let (_t, p, s, h) = setup();
    record(&p, vec![(s.clone(), h, Some(snapshot("w1", "Work")))]).unwrap();
    let first = get(&p, &s);
    let h = Host {
        session: "default".into(),
        socket: "/private/host-two.sock".into(),
    };
    record(&p, vec![(s.clone(), h, Some(snapshot("w1", "Work")))]).unwrap();
    let second = get(&p, &s);
    assert_ne!(first["host_id"], second["host_id"]);
    assert_ne!(first["space"]["id"], second["space"]["id"]);
}
#[test]
fn move_rename_account_and_stopped_history_preserve_identity() {
    let (_t, p, s, h) = setup();
    record(
        &p,
        vec![(s.clone(), h.clone(), Some(snapshot("w1", "Work")))],
    )
    .unwrap();
    let first = get(&p, &s);
    record(
        &p,
        vec![(s.clone(), h.clone(), Some(snapshot("w1", "Renamed")))],
    )
    .unwrap();
    assert_eq!(get(&p, &s)["space"]["id"], first["space"]["id"]);
    let changed =
        record_launch(&p, "work", "conversation", "ignored", s.attachment.clone()).unwrap();
    assert_eq!(changed.id, s.id);
    record(
        &p,
        vec![(changed.clone(), h.clone(), Some(snapshot("w2", "Other")))],
    )
    .unwrap();
    let moved = get(&p, &changed);
    assert_ne!(moved["space"]["id"], first["space"]["id"]);
    assert_eq!(moved["host_id"], first["host_id"]);
    let stopped = record_launch(&p, "work", "conversation", "ignored", None).unwrap();
    let retained = get(&p, &stopped);
    assert_eq!(retained["state"], "last_known");
    assert_eq!(retained["space"], moved["space"]);
    record(&p, vec![(changed, h, Some(snapshot("w9", "Racing")))]).unwrap();
    assert_eq!(get(&p, &stopped)["space"], moved["space"]);
}
#[test]
fn outage_invalid_snapshot_and_expiry_never_refresh_evidence() {
    let (_t, p, s, h) = setup();
    record(
        &p,
        vec![(s.clone(), h.clone(), Some(snapshot("w1", "Work")))],
    )
    .unwrap();
    let first = get(&p, &s);
    record(&p, vec![(s.clone(), h.clone(), None)]).unwrap();
    let v = get(&p, &s);
    assert_eq!(v["state"], "last_known");
    assert_eq!(v["observed_at"], first["observed_at"]);
    let mut bad = snapshot("w1", "Wrong");
    bad.1["tabs"][0]["workspace_id"] = json!("w2");
    record(&p, vec![(s.clone(), h, Some(bad))]).unwrap();
    assert_eq!(get(&p, &s)["space"], first["space"]);
    let mut data = read(&p).unwrap();
    let e = data.sessions.get_mut(&s.id).unwrap();
    e.reason = None;
    e.observed_at -= 60;
    atomic_write(&file(&p), &serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(get(&p, &s)["reason"], "observation_expired");
}
#[test]
fn delayed_batch_does_not_extend_snapshot_freshness() {
    let (_t, p, s, h) = setup();
    let mut old = snapshot("w1", "Work");
    old.0 -= 60;
    let at = old.0;
    record(&p, vec![(s.clone(), h, Some(old))]).unwrap();
    let v = get(&p, &s);
    assert_eq!(v["observed_at"], at);
    assert_eq!(v["valid_until"], at + 30);
    assert_eq!(v["state"], "last_known");
}
#[test]
fn changed_attachment_unobserved_and_tmux_are_not_guessed() {
    let (_t, p, s, h) = setup();
    assert_eq!(get(&p, &s)["state"], "unobserved");
    record(&p, vec![(s.clone(), h, Some(snapshot("w1", "Work")))]).unwrap();
    let mut changed = s.clone();
    changed.attachment.as_mut().unwrap().process_start += 1;
    assert_eq!(get(&p, &changed)["reason"], "attachment_changed");
    let mut tmux = s;
    tmux.id = "other".into();
    tmux.attachment.as_mut().unwrap().host = "tmux".into();
    assert_eq!(get(&p, &tmux)["reason"], "not_herdr");
}
/// A call that records nothing new neither renames nor fsyncs the file (contract 1.57.0).
#[test]
fn recording_nothing_new_does_not_rewrite_the_file() {
    use std::os::unix::fs::MetadataExt;
    let (_t, p, s, h) = setup();
    let host = |h: &Host| Host {
        session: h.session.clone(),
        socket: h.socket.clone(),
    };
    record(
        &p,
        vec![(s.clone(), host(&h), Some(snapshot("w1", "Work")))],
    )
    .unwrap();
    let first = fs::metadata(file(&p)).unwrap();
    record(&p, vec![]).unwrap();
    let mut moved = s.clone();
    moved.attachment.as_mut().unwrap().process_start += 1;
    record(&p, vec![(moved, host(&h), Some(snapshot("w1", "Other")))]).unwrap();
    assert_eq!(fs::metadata(file(&p)).unwrap().ino(), first.ino());
    assert_eq!(get(&p, &s)["space"]["label"], "Work");
    let at = daemon::now() + 5;
    let mut later = snapshot("w1", "Renamed");
    later.0 = at;
    record(&p, vec![(s.clone(), h, Some(later))]).unwrap();
    assert_ne!(fs::metadata(file(&p)).unwrap().ino(), first.ino());
    assert_eq!(get(&p, &s)["space"]["label"], "Renamed");
}
