use super::*;
#[test]
fn orphaned_deletion_is_recoverable_but_live_worker_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    private_dir(&paths.root.join(crate::layout::ACCOUNT_DELETIONS)).unwrap();
    let r = json!({"id":id,"account_id":"codex:fixture","kind":"account_delete","phase":"checking_active","detail":"checking","updated_at":0,"history_home":"/fixture","source_inode":null,"can_cancel":false,"history_preserved":true});
    atomic_write(
        &record_file(&paths, &id).unwrap(),
        &serde_json::to_vec(&r).unwrap(),
    )
    .unwrap();
    let orphan = status(&paths, &id).unwrap();
    assert_eq!(orphan.phase, "failed");
    assert!(orphan.can_recover);
    let lock = lock_file(
        &record_file(&paths, &id)
            .unwrap()
            .with_extension("worker.lock"),
    )
    .unwrap();
    lock.lock_exclusive().unwrap();
    let live = status(&paths, &id).unwrap();
    assert_eq!(live.phase, "checking_active");
    assert!(!live.can_recover);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(record_file(&paths, &id).unwrap()).unwrap())
            .unwrap(),
        r,
        "read projection never rewrites a receipt"
    );
}
#[tokio::test]
async fn replaced_account_home_and_symlink_parent_are_rejected_before_data_changes() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("root"))).unwrap();
    let home = paths.account("disposable").unwrap();
    private_dir(&home).unwrap();
    fs::write(home.join("auth.json"), "synthetic credential").unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut r = Record {
        id: id.clone(),
        account_id: "codex:disposable".into(),
        kind: "account_delete".into(),
        phase: "admitted".into(),
        detail: String::new(),
        created_at: None,
        updated_at: 0,
        history_home: paths
            .root
            .join(crate::layout::PRESERVED_CONVERSATIONS)
            .join(id)
            .join("home"),
        source_inode: Some(fs::metadata(&home).unwrap().ino().wrapping_add(1)),
        can_cancel: false,
        can_recover: false,
        history_preserved: false,
        external_home: None,
        home_disposition: None,
    };
    assert!(
        perform(&paths, &mut r)
            .await
            .unwrap_err()
            .to_string()
            .contains("ACCOUNT_HOME_REPLACED")
    );
    assert_eq!(
        fs::read(home.join("auth.json")).unwrap(),
        b"synthetic credential"
    );
    assert!(!r.history_home.exists());
    assert!(!paths.root.join(crate::layout::REGISTRY_DB).exists());
    fs::rename(
        paths.root.join("accounts"),
        paths.root.join("retained_accounts"),
    )
    .unwrap();
    let outside = d.path().join("outside");
    fs::create_dir_all(outside.join("codex/disposable")).unwrap();
    fs::write(
        outside.join("codex/disposable/auth.json"),
        "outside credential",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, paths.root.join("accounts")).unwrap();
    assert!(
        perform(&paths, &mut r)
            .await
            .unwrap_err()
            .to_string()
            .contains("UNSAFE_ACCOUNT_PARENT")
    );
    assert_eq!(
        fs::read(outside.join("codex/disposable/auth.json")).unwrap(),
        b"outside credential"
    );
    assert!(
        paths
            .root
            .join("retained_accounts/codex/disposable/auth.json")
            .is_file()
    );
}
#[test]
fn interrupted_history_move_retains_both_halves_and_recovers_without_credentials() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("account");
    let vault = d.path().join("vault/home");
    fs::create_dir_all(home.join("sessions")).unwrap();
    fs::create_dir_all(home.join("archived_sessions")).unwrap();
    private_dir(&vault).unwrap();
    fs::write(home.join("sessions/a.jsonl"), "saved").unwrap();
    fs::write(home.join("archived_sessions/b.jsonl"), "archived").unwrap();
    fs::write(home.join("auth.json"), "secret").unwrap();
    fs::rename(home.join("sessions"), vault.join("sessions")).unwrap();
    preserve(&home, &vault).unwrap();
    assert_eq!(fs::read(vault.join("sessions/a.jsonl")).unwrap(), b"saved");
    assert_eq!(
        fs::read(vault.join("archived_sessions/b.jsonl")).unwrap(),
        b"archived"
    );
    assert!(home.join("auth.json").is_file());
    assert!(!vault.join("auth.json").exists());
    assert!(!history_entry("state_secret.sqlite"));
    assert!(history_entry("state_5.sqlite-wal"));
}
#[test]
fn history_move_preserves_native_data_and_excludes_credentials() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("account");
    fs::create_dir_all(home.join("sessions")).unwrap();
    fs::create_dir_all(home.join("archived_sessions")).unwrap();
    for (p, b) in [
        ("auth.json", "secret"),
        ("state_5.sqlite", "native metadata"),
        ("sessions/conversation.jsonl", "conversation"),
        ("archived_sessions/old.jsonl", "archive"),
    ] {
        fs::write(home.join(p), b).unwrap();
    }
    let vault = d.path().join("vault/home");
    preserve(&home, &vault).unwrap();
    assert!(!vault.join("auth.json").exists());
    assert_eq!(
        fs::read(vault.join("sessions/conversation.jsonl")).unwrap(),
        b"conversation"
    );
    assert_eq!(
        fs::read(vault.join("state_5.sqlite")).unwrap(),
        b"native metadata"
    );
    fs::remove_dir_all(home).unwrap();
    assert!(vault.join("archived_sessions/old.jsonl").exists());
}

#[test]
fn deleting_a_linked_claude_account_removes_only_the_links() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let (home, _) = crate::provider::claude::create_home(&paths, "a").unwrap();
    crate::shared_state::enable(&paths.root).unwrap();
    crate::shared_state::ensure_links(&paths, &home).unwrap();
    fs::create_dir_all(home.join("projects/-w")).unwrap();
    fs::write(home.join("projects/-w/c.jsonl"), "transcript").unwrap();
    fs::create_dir_all(home.join("session-env/c")).unwrap();
    fs::write(home.join("session-env/c/e"), "env").unwrap();
    fs::write(home.join("history.jsonl"), "prompts").unwrap();
    fs::write(home.join(".credentials.json"), "x").unwrap();
    let vault = paths.root.join("vault");
    preserve_claude(&home, &vault).unwrap();
    assert!(
        !crate::shared_state::any_linked(&home),
        "links are gone from the home"
    );
    assert!(vault.join("history.jsonl").exists());
    assert!(
        !vault.join("projects").exists(),
        "nothing is moved out of the store"
    );
    let store = crate::shared_state::store(&paths.root);
    assert_eq!(
        fs::read(store.join("projects/-w/c.jsonl")).unwrap(),
        b"transcript"
    );
    fs::remove_dir_all(&home).unwrap();
    assert_eq!(fs::read(store.join("session-env/c/e")).unwrap(), b"env");
    assert_eq!(
        fs::read(store.join("projects/-w/c.jsonl")).unwrap(),
        b"transcript"
    );

    // A foreign link is still refused, and what it points to is untouched.
    let (home, _) = crate::provider::claude::create_home(&paths, "b").unwrap();
    let outside = paths.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), "k").unwrap();
    std::os::unix::fs::symlink(&outside, home.join("projects")).unwrap();
    let error = preserve_claude(&home, &paths.root.join("vault2")).unwrap_err();
    assert!(format!("{error:#}").contains("HISTORY_SYMLINK_UNVERIFIED"));
    assert!(outside.join("keep").exists());
}
