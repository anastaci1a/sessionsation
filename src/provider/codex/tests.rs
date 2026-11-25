use super::*;
use serde_json::{Value, json};

fn read(home: &std::path::Path) -> Value {
    serde_json::from_slice(&std::fs::read(home.join(DAEMON_SETTINGS)).unwrap()).unwrap()
}

#[test]
fn pins_missing_settings_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    assert!(pin_daemon_release(dir.path()).unwrap());
    assert_eq!(
        read(dir.path()),
        json!({"updater":{"autoUpdateEnabled":false}})
    );
    assert!(!pin_daemon_release(dir.path()).unwrap());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.path().join(DAEMON_SETTINGS))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn pin_preserves_other_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("app-server-daemon")).unwrap();
    std::fs::write(
        dir.path().join(DAEMON_SETTINGS),
        br#"{"remoteControlEnabled":true,"featureOverrides":{"x":true},"shutdownGraceSeconds":30,"updater":{"autoUpdateEnabled":true,"updateIntervalMinutes":15}}"#,
    )
    .unwrap();
    assert!(pin_daemon_release(dir.path()).unwrap());
    assert_eq!(
        read(dir.path()),
        json!({"remoteControlEnabled":true,"featureOverrides":{"x":true},"shutdownGraceSeconds":30,"updater":{"autoUpdateEnabled":false,"updateIntervalMinutes":15}})
    );
}

#[test]
fn pin_leaves_unreadable_settings_untouched() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("app-server-daemon")).unwrap();
    for bad in [&b"{not json"[..], b"[]", br#"{"updater":3}"#] {
        std::fs::write(dir.path().join(DAEMON_SETTINGS), bad).unwrap();
        assert!(pin_daemon_release(dir.path()).is_err());
        assert_eq!(
            std::fs::read(dir.path().join(DAEMON_SETTINGS)).unwrap(),
            bad
        );
    }
}
