use super::super::*;
#[test]
fn relocate_archive_restore_keeps_the_relocated_directory() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let old = d.path().join("old");
    let new = d.path().join("new");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    let thread = uuid::Uuid::new_v4().to_string();
    // Relocation completed: the effective directory is the new one.
    relocation::save_location(&paths, &thread, &new).unwrap();
    let source: Instance = serde_json::from_value(json!({"inactive":true,"pane":"","tmux_session":"","pid":0,"start_time":0,"cwd":new,"thread_id":thread,"rollout":"/tmp/r.jsonl","home":"/tmp/h","binary":"codex","resume_options":[]})).unwrap();
    let session = crate::catalog::record_launch(&paths, "acct", &thread, "s", None).unwrap();
    let a = Archive {
        version: 1,
        session,
        source,
        state: "archived".into(),
        archived_at: 1,
        bundle: uuid::Uuid::new_v4().to_string(),
        blobs: vec![],
        thread_ids: [thread.clone()].into(),
        facts: Value::Null,
    };
    // The per-thread record reverted to the original directory (or was lost)
    // while archived; restore must bring the archived effective directory back.
    relocation::save_location(&paths, &thread, &old).unwrap();
    keep_location(&paths, &a).unwrap();
    assert_eq!(
        relocation::saved_location(&paths, &thread).as_deref(),
        new.to_str()
    );
    fs::remove_file(
        paths
            .root
            .join(crate::layout::THREAD_LOCATIONS)
            .join(format!("{thread}.json")),
    )
    .unwrap();
    keep_location(&paths, &a).unwrap();
    assert_eq!(
        relocation::saved_location(&paths, &thread).as_deref(),
        new.to_str()
    );
    // A vanished directory is never written.
    fs::remove_dir(&new).unwrap();
    fs::remove_file(
        paths
            .root
            .join(crate::layout::THREAD_LOCATIONS)
            .join(format!("{thread}.json")),
    )
    .unwrap();
    keep_location(&paths, &a).unwrap();
    assert!(relocation::saved_location(&paths, &thread).is_none());
}
