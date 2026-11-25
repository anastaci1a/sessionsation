use super::super::*;
use std::ffi::OsString;
use std::fs;

#[test]
fn a_share_path_entry_becomes_the_disk_entry_in_order_without_duplicates() {
    let t = tempfile::tempdir().unwrap();
    let disk = t.path().join("disk1");
    let share = t.path().join("user");
    for d in ["live/packages/a/bin", "live/packages/b/bin", "other/bin"] {
        fs::create_dir_all(disk.join(d)).unwrap();
    }
    // The share is the same directory tree (a symlink stands in for the FUSE view).
    std::os::unix::fs::symlink(&disk, &share).unwrap();
    // A second tree under the share that is NOT the disk one.
    let lone = t.path().join("lone");
    fs::create_dir_all(lone.join("x/bin")).unwrap();
    let join = |parts: Vec<PathBuf>| std::env::join_paths(parts).unwrap();
    let list = join(vec![
        "/usr/local/bin".into(),
        share.join("live/packages/a/bin"),
        disk.join("live/packages/a/bin"), // duplicate after rewriting
        share.join("live/packages/b/bin"),
        share.join("missing/bin"), // no disk twin: stays
        "/usr/bin".into(),
        "/usr/local/bin".into(), // plain duplicate
        share.join("live/packages/b/bin"),
    ]);
    let got = normalize_search_path(&list, &share, &disk);
    let want = join(vec![
        "/usr/local/bin".into(),
        disk.join("live/packages/a/bin"),
        disk.join("live/packages/b/bin"),
        share.join("missing/bin"),
        "/usr/bin".into(),
    ]);
    assert_eq!(got, want);
    // Different directories with the same relative path are not rewritten.
    let other_share = t.path().join("share2");
    fs::create_dir_all(other_share.join("other/bin")).unwrap();
    let list: OsString = join(vec![other_share.join("other/bin")]);
    assert_eq!(normalize_search_path(&list, &other_share, &disk), list);
}
