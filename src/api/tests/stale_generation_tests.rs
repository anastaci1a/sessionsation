use super::super::*;

#[test]
fn a_lifecycle_operation_refreshes_the_cached_generation_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let conversation = uuid::Uuid::new_v4().to_string();
    let s = catalog::record_launch(&paths, "a", &conversation, "work", None).unwrap();
    let old = s.generation();
    let snapshot = json!({"observed_at":daemon::now(),"accounts":[],
        "sessions":[{"id":s.id,"generation":old},{"id":"other","generation":"x"}]});
    write_snapshot(&paths, ArchiveFilter::Active, &snapshot);
    let renamed = catalog::rename(&paths, &s.id, &old, "work2").unwrap();
    assert_ne!(renamed.generation(), old);
    // Stale until the lifecycle hook runs.
    assert_eq!(
        read_snapshot(&paths, ArchiveFilter::Active).unwrap()["sessions"][0]["generation"],
        old.as_str()
    );
    patch_snapshot_generation(&paths, &s.id);
    let fresh = read_snapshot(&paths, ArchiveFilter::Active).unwrap();
    assert_eq!(
        fresh["sessions"][0]["generation"],
        renamed.generation().as_str()
    );
    assert_eq!(fresh["sessions"][1]["generation"], "x");
}
