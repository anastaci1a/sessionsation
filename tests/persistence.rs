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
#[test]
fn legacy_accounts_are_migrated_to_codex_once_with_an_event() {
    use sessionsation::{provider::ProviderId, store::LEGACY_CODEX_PROVIDER_EVENT};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    drop(Store::open(&paths).unwrap());
    // A registry written before providers existed: no `provider` field.
    let mut legacy = serde_json::to_value(State::default()).unwrap();
    for name in ["claude", "work"] {
        let mut account = serde_json::to_value(sessionsation::model::Account::new(
            name.into(),
            String::new(),
        ))
        .unwrap();
        account.as_object_mut().unwrap().remove("provider");
        legacy["accounts"][name] = account;
    }
    let db = rusqlite::Connection::open(paths.root.join("registry.db")).unwrap();
    db.execute("INSERT INTO state VALUES(1,?1)", [legacy.to_string()])
        .unwrap();
    let events = |db: &rusqlite::Connection| -> i64 {
        db.query_row(
            "SELECT COUNT(*) FROM events WHERE kind=?1",
            [LEGACY_CODEX_PROVIDER_EVENT],
            |r| r.get(0),
        )
        .unwrap()
    };
    // Read-only snapshots migrate in memory without writing.
    let snapshot = sessionsation::store::load_state_snapshot(&paths).unwrap();
    assert_eq!(snapshot.accounts["claude"].provider, ProviderId::Codex);
    assert_eq!(events(&db), 0);
    for _ in 0..2 {
        let store = Store::open(&paths).unwrap();
        let state = store.load().unwrap();
        // Never inferred from a name: an account named "claude" stays Codex.
        assert!(
            state
                .accounts
                .values()
                .all(|a| a.provider == ProviderId::Codex)
        );
        drop(store);
        assert_eq!(events(&db), 2, "one event per account, written once");
    }
    let stored: String = db
        .query_row("SELECT json FROM state WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let stored: serde_json::Value = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored["accounts"]["work"]["provider"], "codex");
    let mut legacy = stored.clone();
    assert!(sessionsation::store::migrate_legacy_codex_provider(&mut legacy).is_empty());
}
