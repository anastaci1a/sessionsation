use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn seeds_once_keeps_keys_and_mode() {
    let d = tempfile::tempdir().unwrap();
    let f = settings_file(d.path());
    fs::write(&f, r#"{"model":"opus"}"#).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(default_new(d.path(), "bash x.sh").unwrap());
    let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    assert_eq!(
        v,
        json!({"model":"opus","statusLine":{"type":"command","command":"bash x.sh"}})
    );
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(!default_new(d.path(), "bash y.sh").unwrap());
}

#[test]
fn empty_disables_and_symlink_refused() {
    let d = tempfile::tempdir().unwrap();
    assert!(!default_new(d.path(), "").unwrap());
    assert!(!settings_file(d.path()).exists());
    let real = d.path().join("real.json");
    fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, settings_file(d.path())).unwrap();
    assert!(default_new(d.path(), "bash x.sh").is_err());
    assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
}
