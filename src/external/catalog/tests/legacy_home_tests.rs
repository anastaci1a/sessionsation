use super::super::*;

fn legacy_and_new(root: &Path) -> (PathBuf, PathBuf) {
    let new = root.join("accounts/codex/gate1");
    fs::create_dir_all(&new).unwrap();
    std::os::unix::fs::symlink("codex/gate1", root.join("accounts/gate1")).unwrap();
    (root.join("accounts/gate1"), new)
}

/// A conversation whose owner still names the legacy path (a symlink after
/// `layout migrate`) is owned by the migrated home, and the other way round.
#[test]
fn owner_matches_across_legacy_symlink_and_new_home() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let (legacy, new) = legacy_and_new(&paths.root);
    let thread = uuid::Uuid::new_v4().to_string();
    update(&paths, &thread, |r| r.owner = Some(legacy.clone())).unwrap();
    assert!(messaging_owner(&paths, &thread, &new).unwrap());
    assert!(messaging_owner(&paths, &thread, &legacy).unwrap());
    update(&paths, &thread, |r| r.owner = Some(new.clone())).unwrap();
    assert!(messaging_owner(&paths, &thread, &legacy).unwrap());
    update(&paths, &thread, |r| {
        r.owner = Some(paths.root.join("accounts/codex/other"))
    })
    .unwrap();
    assert!(!messaging_owner(&paths, &thread, &new).unwrap());
}

/// The reported bug: a binding recorded with the new home, an owner record
/// left at the legacy path; the recorded binding must still be returned.
#[test]
fn recorded_bindings_survive_a_stale_legacy_owner() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let (legacy, new) = legacy_and_new(&paths.root);
    let thread = uuid::Uuid::new_v4().to_string();
    update(&paths, &thread, |r| r.owner = Some(legacy.clone())).unwrap();
    let pid = std::process::id();
    let start = process_info(pid).unwrap().start_time;
    private_dir(&paths.root.join(crate::layout::MANAGED_PANES)).unwrap();
    atomic_write(
        &paths.root.join(crate::layout::MANAGED_PANES).join("7.json"),
        &serde_json::to_vec(&Binding {
            pid,
            start_time: start,
            home: new.clone(),
            thread: thread.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    let found = messaging_recorded_bindings_for_test(&paths);
    assert_eq!(found, vec![(thread, new, pid, start)]);
}
fn messaging_recorded_bindings_for_test(paths: &Paths) -> Vec<(String, PathBuf, u32, u64)> {
    super::super::super::messaging_recorded_bindings(paths).unwrap()
}
