use super::*;

#[test]
fn new_root_is_stamped_and_legacy_root_fails_closed() {
    let t = tempfile::tempdir().unwrap();
    admit(t.path()).unwrap();
    let r = read_record(t.path()).unwrap().unwrap();
    assert_eq!((r.version, r.state), (VERSION, State::Complete));
    let legacy = tempfile::tempdir().unwrap();
    fs::write(legacy.path().join("registry.db"), "").unwrap();
    let e = admit(legacy.path()).unwrap_err().to_string();
    assert!(e.starts_with("LAYOUT_MIGRATION_REQUIRED"), "{e}");
    assert!(!legacy.path().join(LAYOUT_FILE).exists());
}

#[test]
fn deferred_legacy_homes_resolve_until_moved() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    assert_eq!(
        account_home(root, ProviderId::Gemini, "g"),
        root.join("accounts/gemini/g")
    );
    fs::create_dir_all(root.join("gemini-accounts/g")).unwrap();
    fs::create_dir_all(root.join("accounts/work")).unwrap();
    assert_eq!(
        account_home(root, ProviderId::Gemini, "g"),
        root.join("gemini-accounts/g")
    );
    assert_eq!(
        account_home(root, ProviderId::Codex, "work"),
        root.join("accounts/work")
    );
    // A platform directory is never a legacy Codex home.
    fs::create_dir_all(root.join("accounts/claude/x")).unwrap();
    assert_eq!(
        account_home(root, ProviderId::Codex, "claude"),
        root.join("accounts/codex/claude")
    );
    fs::create_dir_all(root.join("accounts/codex/new")).unwrap();
    let names: Vec<_> = codex_homes(root).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["new", "work"]);
    // Once moved, the target wins even though a compatibility link remains.
    fs::rename(
        root.join("gemini-accounts/g"),
        root.join("accounts/gemini-g"),
    )
    .unwrap();
    fs::create_dir_all(root.join("accounts/gemini")).unwrap();
    fs::rename(
        root.join("accounts/gemini-g"),
        root.join("accounts/gemini/g"),
    )
    .unwrap();
    std::os::unix::fs::symlink("../accounts/gemini/g", root.join("gemini-accounts/g")).unwrap();
    assert_eq!(
        account_home(root, ProviderId::Gemini, "g"),
        root.join("accounts/gemini/g")
    );
    assert!(same_home(
        &root.join("gemini-accounts/g"),
        &root.join("accounts/gemini/g")
    ));
}
