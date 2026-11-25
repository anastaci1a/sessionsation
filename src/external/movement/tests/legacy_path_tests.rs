use super::super::*;

/// Codex's `state_5.sqlite` keeps rollout paths under the legacy home path,
/// which `layout migrate` left as a symlink to the new home.
#[test]
fn reported_history_path_accepts_the_legacy_symlink_spelling() {
    let d = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(d.path()).unwrap();
    let new = root.join("accounts/codex/personal");
    fs::create_dir_all(new.join("sessions/2026/10/29")).unwrap();
    fs::create_dir_all(root.join("accounts")).unwrap();
    std::os::unix::fs::symlink("codex/personal", root.join("accounts/personal")).unwrap();
    let rollout = "sessions/2026/10/29/rollout-x.jsonl";
    fs::write(new.join(rollout), "").unwrap();
    let legacy = root.join("accounts/personal").join(rollout);
    // Existing and not-yet-existing files, legacy and canonical spellings.
    for reported in [
        legacy.clone(),
        new.join(rollout),
        new.join("sessions/a/new.jsonl"),
    ] {
        let got = reported_history_path(reported.to_str().unwrap(), &new).unwrap();
        assert!(got.starts_with(&new), "{got:?}");
    }
    assert!(
        reported_history_path(legacy.to_str().unwrap(), &root.join("accounts/codex/work")).is_err()
    );
    let outside = root.join("accounts/personal/auth.json");
    assert!(reported_history_path(outside.to_str().unwrap(), &new).is_err());
    // The source side of a transfer may also be spelled through the symlink.
    let source = root.join("accounts/personal").join(rollout);
    let target = target_path(&source, &new, &root.join("accounts/codex/work")).unwrap();
    assert_eq!(target, root.join("accounts/codex/work").join(rollout));
}
