use super::*;
use std::os::unix::fs::PermissionsExt;
#[test]
fn toggle_edits_only_the_key_keeps_the_mode_and_refuses_symlinks() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    let path = settings_file(home);
    // Absent file: enable creates it 0600; disable is a no-op.
    assert!(!set(home, false).unwrap().changed);
    let t = set(home, true).unwrap();
    assert!(t.changed && t.previous.is_none());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(configured(home), Some(true));
    // Other keys survive; the mode is kept.
    fs::write(
        &path,
        r#"{"general":{"vimMode":false},"experimental":{"other":1}}"#,
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(set(home, true).unwrap().changed);
    assert!(!set(home, true).unwrap().changed);
    let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"general":{"vimMode":false},"experimental":{"other":1,"modelSteering":true}})
    );
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let t = set(home, false).unwrap();
    assert!(t.changed && t.previous == Some(Value::Bool(true)));
    let v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        v,
        serde_json::json!({"general":{"vimMode":false},"experimental":{"other":1}})
    );
    assert_eq!(configured(home), None);
    // Comments (JSONC), a non-object 'experimental' and symlinks are refused unchanged.
    fs::write(&path, "{ // note\n}").unwrap();
    assert!(set(home, true).is_err());
    fs::write(&path, r#"{"experimental":true}"#).unwrap();
    assert!(set(home, true).is_err());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        r#"{"experimental":true}"#
    );
    let real = home.join("real.json");
    fs::write(&real, "{}").unwrap();
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();
    let e = set(home, true).unwrap_err().to_string();
    assert!(e.starts_with("SETTINGS_IS_SYMLINK"), "{e}");
    assert_eq!(fs::read_to_string(&real).unwrap(), "{}");
}
#[test]
fn the_effective_value_follows_layers_and_needs_a_restart_after_a_change() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let work = d.path().join("work");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(work.join(".gemini")).unwrap();
    let none: [(String, String); 2] = [
        (
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH".into(),
            d.path().join("absent-system.json").display().to_string(),
        ),
        (
            "GEMINI_CLI_SYSTEM_DEFAULTS_PATH".into(),
            d.path().join("absent-defaults.json").display().to_string(),
        ),
    ];
    let later = crate::daemon::now() + 100;
    let e = effective(&home, Some(&work), Some(&none), Some(later));
    assert_eq!((e.enabled, e.source), (Some(false), "default"));
    set(&home, true).unwrap();
    let e = effective(&home, Some(&work), Some(&none), Some(later));
    assert_eq!((e.enabled, e.source), (Some(true), "user"));
    // Changed after the process started: unknown until a restart.
    let e = effective(&home, Some(&work), Some(&none), Some(1));
    assert!(e.restart_required);
    assert_eq!((e.enabled, e.reason), (None, Some(RESTART_REQUIRED)));
    // A disagreeing workspace value depends on folder trust: unknown.
    fs::write(
        work.join(".gemini/settings.json"),
        r#"{"experimental":{"modelSteering":false}}"#,
    )
    .unwrap();
    let e = effective(&home, Some(&work), Some(&none), Some(later));
    assert_eq!((e.enabled, e.reason), (None, Some(UNKNOWN)));
    // A system file overrides everything below it.
    let system = d.path().join("system.json");
    fs::write(&system, r#"{"experimental":{"modelSteering":true}}"#).unwrap();
    let env = [
        (
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH".to_owned(),
            system.display().to_string(),
        ),
        none[1].clone(),
    ];
    let e = effective(&home, Some(&work), Some(&env), Some(later));
    assert_eq!((e.enabled, e.source), (Some(true), "system"));
}
