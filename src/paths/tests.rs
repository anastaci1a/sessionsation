#[test]
fn stale_atomic_write_temps_are_swept_and_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let dir = root
        .path()
        .join(crate::layout::OBSERVATIONS)
        .join("native-turns");
    std::fs::create_dir_all(&dir).unwrap();
    let stale = dir.join(".tmpl2no68");
    let fresh = dir.join(".tmpAbC123");
    let lookalike = dir.join(".tmpl2no68.json");
    let real = dir.join("turn.json");
    let link_target = root.path().join("elsewhere");
    std::fs::write(&link_target, "x").unwrap();
    let link = dir.join(".tmpZZZZZZ");
    std::os::unix::fs::symlink(&link_target, &link).unwrap();
    let outside = root.path().join("accounts/claude/a");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = outside.join(".tmpl2no68");
    for p in [&stale, &fresh, &lookalike, &real, &outside] {
        std::fs::write(p, "x").unwrap();
    }
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    for p in [&stale, &lookalike, &real, &outside] {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    assert_eq!(
        super::sweep_stale_temps(root.path(), super::STALE_TEMP_AGE),
        1
    );
    assert!(!stale.exists());
    assert!(fresh.exists(), "recent temp may belong to a live writer");
    assert!(lookalike.exists() && real.exists());
    assert!(std::fs::symlink_metadata(&link).is_ok() && link_target.exists());
    assert!(outside.exists(), "account homes are not swept");
}
#[test]
fn stable_worker_entries_recover_journals_omitted_by_readdir() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut entries = vec![];
    for (id, suffix) in [
        ("move", ".worker-lock"),
        ("resume", ".worker.lock"),
        ("legacy", ".log"),
        ("space", ".lock"),
    ] {
        let journal = dir.join(format!("{id}.json"));
        std::fs::write(&journal, "{}").unwrap();
        let anchor = dir.join(format!("{id}{suffix}"));
        std::fs::write(&anchor, "").unwrap();
        entries.push(anchor);
    }
    entries.push(dir.join("queue.lock"));
    entries.push(dir.join("orphan.worker.lock"));
    entries.push(dir.join(".tmp-unpublished"));
    entries.push(dir.join("move.json"));
    let found = super::journal_paths_from_entries(dir, entries);
    assert_eq!(found.len(), 4);
    assert!(found.iter().all(|p| p.extension().unwrap() == "json"));
}
use super::data_root;
use std::path::PathBuf;

#[test]
fn new_account_limit_does_not_reject_readable_legacy_accounts() {
    super::validate_new_account_name(&"x".repeat(30)).unwrap();
    assert!(super::validate_new_account_name(&"x".repeat(31)).is_err());
    assert!(super::validate_new_account_name("non ascii 界").is_err());
    super::validate_name(&"x".repeat(64)).unwrap();
}

#[test]
fn only_explicit_creation_makes_a_missing_root() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("missing");
    let error = super::Paths::open(Some(root.clone()))
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("DATA_ROOT_MISSING"), "{error}");
    assert!(!root.exists());
    let created = super::Paths::new(Some(root.clone())).unwrap();
    assert!(
        created
            .runtime
            .to_string_lossy()
            .starts_with("/tmp/sessionsation-")
    );
    let opened = super::Paths::open(Some(root.clone())).unwrap();
    assert_eq!(opened.root, created.root);
    assert_eq!(opened.runtime, created.runtime);
    std::os::unix::fs::symlink(&root, t.path().join("alias")).unwrap();
    assert!(super::Paths::open(Some(t.path().join("alias"))).is_err());
}
#[test]
fn unraid_defaults_to_persistent_appdata_without_home() {
    assert_eq!(
        data_root(None, None, None, true).unwrap(),
        PathBuf::from("/mnt/user/appdata/sessionsation")
    );
}

#[test]
fn explicit_path_and_environment_override_platform_defaults() {
    for unraid in [false, true] {
        assert_eq!(
            data_root(
                Some("/explicit".into()),
                Some("/environment".into()),
                None,
                unraid
            )
            .unwrap(),
            PathBuf::from("/explicit")
        );
        assert_eq!(
            data_root(None, Some("/environment".into()), None, unraid).unwrap(),
            PathBuf::from("/environment")
        );
    }
}

#[test]
fn other_hosts_keep_the_home_default() {
    assert_eq!(
        data_root(None, None, Some("/home/alice".into()), false).unwrap(),
        PathBuf::from("/home/alice/.local/share/sessionsation")
    );
    assert!(data_root(None, None, None, false).is_err());
}
