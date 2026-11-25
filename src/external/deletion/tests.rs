use super::*;

#[test]
fn legacy_symlink_and_migrated_home_are_one_home() {
    let d = tempfile::tempdir().unwrap();
    let new = d.path().join("accounts/codex/personal");
    fs::create_dir_all(&new).unwrap();
    std::os::unix::fs::symlink("codex/personal", d.path().join("accounts/personal")).unwrap();
    let legacy = d.path().join("accounts/personal");
    let set = canonical_homes([new.clone(), legacy.clone(), legacy.join("")]);
    assert_eq!(set.len(), 1, "{set:?}");
    assert_eq!(
        set.into_iter().next().unwrap(),
        fs::canonicalize(new).unwrap()
    );
}
