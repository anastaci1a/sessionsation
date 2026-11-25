use super::super::*;
use std::sync::atomic::Ordering;
#[tokio::test]
async fn refreshes_never_overlap_and_leave_a_fresh_snapshot_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    // A snapshot another process wrote within the period is not rebuilt.
    write_snapshot(
        &paths,
        ArchiveFilter::Active,
        &json!({"observed_at":daemon::now(),"sessions":[]}),
    );
    let before = SNAPSHOT_BUILDS.load(Ordering::Acquire);
    assert!(!refresh_snapshot_if_due(&paths).await);
    assert_eq!(SNAPSHOT_BUILDS.load(Ordering::Acquire), before);
    // An old one is rebuilt, once, however many callers ask at the same time.
    write_snapshot(
        &paths,
        ArchiveFilter::Active,
        &json!({"observed_at":daemon::now() - 3600,"sessions":[]}),
    );
    let (a, b, c) = tokio::join!(
        refresh_snapshot_if_due(&paths),
        refresh_snapshot_if_due(&paths),
        refresh_snapshot_if_due(&paths)
    );
    assert_eq!(SNAPSHOT_BUILDS.load(Ordering::Acquire), before + 1);
    assert_eq!([a, b, c].iter().filter(|r| **r).count(), 1);
    let at = read_snapshot(&paths, ArchiveFilter::Active).unwrap()["observed_at"]
        .as_i64()
        .unwrap();
    assert!(daemon::now() - at <= 1, "the snapshot was rewritten");
}
