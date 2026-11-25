use super::super::*;
#[test]
fn resolve_overlays_cached_snapshot_facts_with_their_age() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
    let blank = || json!({"id":"s1","availability":null,"activity":null,"pid":null});
    // No snapshot: everything stays null.
    let mut row = blank();
    overlay_resolve_snapshot(&paths, &mut row, 1000);
    assert!(row["availability"].is_null() && row["snapshot"].is_null());
    let snapshot = json!({"observed_at":990,"sessions":[
        {"id":"other","availability":"stopped","activity":null,"pid":null},
        {"id":"s1","availability":"running","activity":"working","pid":4242,"live_activity":{"state":"working"}}]});
    write_snapshot(&paths, ArchiveFilter::Active, &snapshot);
    let mut row = blank();
    overlay_resolve_snapshot(&paths, &mut row, 1000);
    assert_eq!(row["availability"], "running");
    assert_eq!(row["activity"], "working");
    assert_eq!(row["pid"], 4242);
    assert_eq!(row["live_activity"]["state"], "working");
    assert_eq!(row["snapshot"]["observed_at"], 990);
    assert_eq!(row["snapshot"]["age_seconds"], 10);
    assert_eq!(row["snapshot"]["fresh"], true);
    // A session missing from every snapshot keeps null.
    let mut row = json!({"id":"absent"});
    overlay_resolve_snapshot(&paths, &mut row, 1000);
    assert!(row["availability"].is_null() && row["pid"].is_null() && row["snapshot"].is_null());
    // An archived session is found in the `all` snapshot; an old snapshot is not fresh.
    write_snapshot(
        &paths,
        ArchiveFilter::All,
        &json!({"observed_at":100,"sessions":[
        {"id":"absent","availability":"archived","activity":null,"pid":null}]}),
    );
    let mut row = json!({"id":"absent"});
    overlay_resolve_snapshot(&paths, &mut row, 1000);
    assert_eq!(row["availability"], "archived");
    assert_eq!(row["snapshot"]["fresh"], false);
}
