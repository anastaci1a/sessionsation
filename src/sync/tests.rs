use super::*;

#[test]
fn shared_holders_coexist_and_exclude_an_exclusive_one() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().to_path_buf(),
        runtime: dir.path().join("run"),
    };
    fs::create_dir_all(paths.root.join("messaging")).unwrap();
    let a = Lock::open(&paths, LockKind::Adapter).unwrap();
    let b = Lock::open(&paths, LockKind::Adapter).unwrap();
    let c = Lock::open(&paths, LockKind::Adapter).unwrap();
    assert!(a.try_shared().unwrap());
    assert!(b.try_shared().unwrap());
    assert!(!c.try_exclusive().unwrap());
    drop(a);
    drop(b);
    assert!(c.try_exclusive().unwrap());
    let d = Lock::open(&paths, LockKind::Adapter).unwrap();
    assert!(!d.try_shared().unwrap());
    assert_eq!(d.kind(), LockKind::Adapter);
}
