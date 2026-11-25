use super::*;
#[test]
fn parallel_metadata_preserves_order_hidden_files_and_symlinks_without_following() {
    use std::os::unix::fs::symlink;
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("home");
    fs::create_dir_all(root.join("a/sub")).unwrap();
    fs::write(root.join("a/sub/history.jsonl"), b"history").unwrap();
    fs::write(root.join("a.txt"), b"data").unwrap();
    fs::write(root.join(".hidden"), b"private").unwrap();
    let outside = d.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("must-not-scan"), b"secret").unwrap();
    symlink(&outside, root.join("linked")).unwrap();
    let mut first = vec![];
    files(&root, &root, &mut first).unwrap();
    let mut again = vec![];
    files(&root, &root, &mut again).unwrap();
    assert_eq!(first, again);
    let paths: Vec<_> = first.iter().map(|v| v["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        vec![".hidden", "a/sub/history.jsonl", "a.txt", "linked"]
    );
    fs::write(root.join("a.txt"), b"changed").unwrap();
    let mut changed = vec![];
    files(&root, &root, &mut changed).unwrap();
    assert_ne!(first, changed);
}
#[tokio::test]
async fn concurrent_launches_share_membership_but_exclude_removal() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let a = launch_lock(&p, ProviderId::Codex, "test", || {})
        .await
        .unwrap();
    let b = launch_lock(&p, ProviderId::Codex, "test", || {})
        .await
        .unwrap();
    assert!(operation_lock(&p, ProviderId::Codex, "test").is_err());
    FileExt::unlock(&a).unwrap();
    FileExt::unlock(&b).unwrap();
    drop(a);
    drop(b);
    let removal = operation_lock(&p, ProviderId::Codex, "test").unwrap();
    let short = std::time::Duration::from_millis(100);
    assert!(
        launch_lock_within(&p, ProviderId::Codex, "test", short, || {})
            .await
            .is_err()
    );
    drop(removal);
    assert!(
        launch_lock_within(&p, ProviderId::Codex, "test", short, || {})
            .await
            .is_ok()
    );
}
#[test]
fn removal_membership_never_erases_or_disables_provider_data() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let home = p.account("test").unwrap();
    private_dir(&home).unwrap();
    fs::write(home.join("history.jsonl"), b"recoverable fixture").unwrap();
    let file = marker(&p, "test").unwrap();
    atomic_write(&file, b"{}").unwrap();
    assert!(ensure_available(&p, ProviderId::Codex, "test").is_err());
    restore(&p, ProviderId::Codex, "test").unwrap();
    assert!(ensure_available(&p, ProviderId::Codex, "test").is_ok());
    assert_eq!(
        fs::read(home.join("history.jsonl")).unwrap(),
        b"recoverable fixture"
    );
}

#[tokio::test]
async fn launch_lock_waits_for_an_account_operation_and_times_out_busy() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("data"))).unwrap();
    // Uncontended: immediate, no wait notice.
    let mut waited = false;
    let free = launch_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::from_millis(200),
        || waited = true,
    )
    .await
    .unwrap();
    assert!(!waited);
    drop(free);
    // The exclusive side is unchanged: it refuses while a launch holds the shared lock.
    let shared = launch_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::ZERO,
        || {},
    )
    .await
    .unwrap();
    assert!(
        format!(
            "{:#}",
            operation_lock(&paths, ProviderId::Codex, "acct").unwrap_err()
        )
        .starts_with("ACCOUNT_BUSY")
    );
    drop(shared);
    // Held past the window: certain ACCOUNT_BUSY after waiting the whole window.
    let held = operation_lock(&paths, ProviderId::Codex, "acct").unwrap();
    let start = std::time::Instant::now();
    let mut notices = 0;
    let error = launch_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::from_millis(300),
        || notices += 1,
    )
    .await
    .unwrap_err();
    assert!(start.elapsed() >= std::time::Duration::from_millis(300));
    assert_eq!(notices, 1);
    assert_eq!(
        format!("{error:#}"),
        "ACCOUNT_BUSY: an account operation held the lock for 300ms"
    );
    // Released within the window: the launch proceeds.
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        drop(held);
    });
    let start = std::time::Instant::now();
    let mut notices = 0;
    launch_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::from_secs(20),
        || notices += 1,
    )
    .await
    .unwrap();
    assert!(start.elapsed() >= std::time::Duration::from_millis(350));
    assert_eq!(notices, 1);
    release.join().unwrap();
    assert_eq!(describe_wait(LAUNCH_LOCK_WAIT), "120s");
}

#[tokio::test]
async fn worker_lock_waits_for_a_holder_and_times_out_busy() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("data"))).unwrap();
    // A launch holds the shared side; the exclusive worker waits for it.
    let held = launch_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::ZERO,
        || {},
    )
    .await
    .unwrap();
    let start = std::time::Instant::now();
    let mut notices = 0;
    let error = operation_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::from_millis(300),
        || notices += 1,
    )
    .await
    .unwrap_err();
    assert!(start.elapsed() >= std::time::Duration::from_millis(300));
    assert_eq!(notices, 1);
    assert_eq!(
        format!("{error:#}"),
        "ACCOUNT_BUSY: an account operation held the lock for 300ms"
    );
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        drop(held);
    });
    let mut notices = 0;
    let lock = operation_lock_within(
        &paths,
        ProviderId::Codex,
        "acct",
        std::time::Duration::from_secs(20),
        || notices += 1,
    )
    .await
    .unwrap();
    assert_eq!(notices, 1);
    release.join().unwrap();
    // Exclusive once acquired: nothing else gets in.
    assert!(
        launch_lock_within(
            &paths,
            ProviderId::Codex,
            "acct",
            std::time::Duration::ZERO,
            || {}
        )
        .await
        .is_err()
    );
    drop(lock);
}

pub(super) fn marker(paths: &Paths, name: &str) -> Result<std::path::PathBuf> {
    marker_for(paths, ProviderId::Codex, name)
}
