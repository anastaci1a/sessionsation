use sessionsation::{
    config::Config,
    model::State,
    paths::{Paths, validate_name},
    store::{Store, load_config, save_config},
};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn registry_survives_restart_and_excludes_second_writer() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let mut store = Store::open(&paths).unwrap();
    assert!(Store::open(&paths).is_err());
    let mut s = State::default();
    s.accounts.insert(
        "a".into(),
        sessionsation::model::Account::new("a".into(), "identity".into()),
    );
    store.save(&s, 1000, "test", "a").unwrap();
    drop(store);
    let store = Store::open(&paths).unwrap();
    assert_eq!(store.load().unwrap().accounts["a"].identity, "identity");
}
#[test]
fn invalid_config_does_not_replace_previous_config() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let mut c = Config::default();
    save_config(&p, &c).unwrap();
    c.weekly_ceiling = 101.;
    assert!(save_config(&p, &c).is_err());
    assert_eq!(load_config(&p).unwrap().weekly_ceiling, 90.);
}
#[test]
fn legacy_freshness_interval_is_migrated_past_the_idle_poll() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let mut old = serde_json::to_value(Config::default()).unwrap();
    old["stale_seconds"] = serde_json::json!(120);
    std::fs::write(
        p.root.join("config.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    let loaded = load_config(&p).unwrap();
    assert_eq!(loaded.stale_seconds, 210);
    assert!(loaded.validate().is_ok());
}
#[test]
fn directories_and_files_are_private_even_with_permissive_umask() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    save_config(&p, &Config::default()).unwrap();
    assert_eq!(
        std::fs::metadata(&p.root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(p.root.join("config.json"))
            .unwrap()
            .mode()
            & 0o777,
        0o600
    );
}
#[test]
fn paths_cannot_escape_account_or_session_directories() {
    for s in ["", "../a", "a/b", "a b", ".", "a\n"] {
        assert!(validate_name(s).is_err());
    }
}
#[test]
fn symlink_state_root_is_rejected() {
    let t = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(t.path(), t.path().join("alias")).unwrap();
    assert!(Paths::new(Some(t.path().join("alias"))).is_err());
}
