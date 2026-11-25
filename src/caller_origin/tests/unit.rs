use super::super::*;

#[test]
fn walks_the_chain_and_checks_start_time() {
    let table = |pid: u32| match pid {
        50 => Some((40, 500)),
        40 => Some((30, 400)),
        30 => Some((1, 300)),
        _ => None,
    };
    assert!(chain_hits(50, &[(30, 300)], table));
    // pid reuse: same pid, different start time.
    assert!(!chain_hits(50, &[(30, 999)], table));
    assert!(!chain_hits(50, &[(77, 300)], table));
    assert!(chain_hits(40, &[(40, 400)], table));
    assert!(!chain_hits(999, &[(30, 300)], table));
}

#[test]
fn a_parent_loop_terminates() {
    assert!(!chain_hits(5, &[(9, 9)], |p| Some((p % 2 + 2, 1))));
}

#[test]
fn explicit_origin_wins() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().to_path_buf())).unwrap();
    assert_eq!(
        resolve(&paths, Some("operator")),
        ("operator".into(), "explicit")
    );
    assert_eq!(resolve(&paths, None), ("operator".into(), "default"));
}
