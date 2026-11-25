use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

#[test]
fn read_write_preserve_mode_and_symlink() {
    let d = tempfile::tempdir().unwrap();
    let h = d.path();
    assert_eq!(skip(h), None);
    assert_eq!(row(h)["bypass_warning"], "show");
    let f = settings_file(h);
    fs::write(&f, r#"{"model":"opus","crossSessionInbound":"accept"}"#).unwrap();
    fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
    let t = set(h, true).unwrap();
    assert!(t.changed && t.previous.is_none());
    assert_eq!(skip(h), Some(true));
    assert_eq!(row(h)["bypass_warning"], "skip");
    let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    assert_eq!(v["model"], "opus");
    assert_eq!(v["crossSessionInbound"], "accept");
    assert_eq!(
        fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(!set(h, true).unwrap().changed);
    let t = set(h, false).unwrap();
    assert!(t.changed && t.previous == Some(true));
    assert_eq!(skip(h), Some(false));
    assert_eq!(row(h)["bypass_warning"], "show");
    let v: Value = serde_json::from_slice(&fs::read(&f).unwrap()).unwrap();
    assert_eq!(v["model"], "opus");
}

#[test]
fn symlink_refused() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().join("real.json");
    fs::write(&real, "{}").unwrap();
    std::os::unix::fs::symlink(&real, d.path().join("settings.json")).unwrap();
    let e = set(d.path(), true).unwrap_err().to_string();
    assert!(e.contains("SETTINGS_IS_SYMLINK"), "{e}");
    assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
}

#[test]
fn new_account_default_respects_existing() {
    let d = tempfile::tempdir().unwrap();
    assert!(default_new(d.path()).unwrap());
    assert_eq!(skip(d.path()), Some(true));
    let e = tempfile::tempdir().unwrap();
    set(e.path(), false).unwrap();
    assert!(!default_new(e.path()).unwrap());
    assert_eq!(skip(e.path()), Some(false));
}
