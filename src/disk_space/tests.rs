use super::*;

#[test]
fn refuses_below_the_floor_with_both_figures() {
    let dir = tempfile::tempdir().unwrap();
    let err = check(dir.path(), 300 * MB, "archive", |_| Some(100 * MB)).unwrap_err();
    let s = err.downcast_ref::<InsufficientSpace>().unwrap();
    assert_eq!((s.free_bytes, s.needed_bytes), (100 * MB, 300 * MB));
    assert!(
        err.to_string()
            .starts_with("INSUFFICIENT_DISK_SPACE: archive")
    );
    assert!(check(dir.path(), 300 * MB, "archive", |_| Some(300 * MB)).is_ok());
    // Disabled, or an unreadable figure, never blocks.
    assert!(check(dir.path(), 0, "archive", |_| Some(0)).is_ok());
    assert!(check(dir.path(), 300 * MB, "archive", |_| None).is_ok());
}

#[test]
fn a_missing_root_is_measured_through_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("not/yet/there");
    let err = check(&root, 10 * MB, "launch", |p| {
        assert_eq!(p, dir.path());
        Some(MB)
    })
    .unwrap_err();
    assert!(err.downcast_ref::<InsufficientSpace>().is_some());
}

#[test]
fn enospc_is_found_anywhere_in_the_chain() {
    let raw = std::io::Error::from_raw_os_error(libc::ENOSPC);
    let wrapped = anyhow::Error::new(raw)
        .context("write snapshot")
        .context("state");
    assert!(is_disk_full(&wrapped));
    assert!(!is_disk_full(&anyhow!("other")));
    let quota = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::EDQUOT));
    assert!(is_disk_full(&quota));
    assert!(!is_disk_full(&anyhow::Error::new(
        std::io::Error::from_raw_os_error(libc::EACCES)
    )));
}

#[test]
fn the_real_filesystem_reports_a_figure() {
    let dir = tempfile::tempdir().unwrap();
    assert!(free_bytes(dir.path()).is_some());
}
