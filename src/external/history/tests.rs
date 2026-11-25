use super::*;

#[tokio::test]
async fn deletion_reuses_original_native_proofs_and_rejects_changed_rollouts() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("account");
    let vault = d.path().join("preserved/home");
    fs::create_dir_all(home.join("sessions")).unwrap();
    let relative = Path::new("sessions/saved.jsonl");
    let file = home.join(relative);
    fs::write(
        &file,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"test","history_mode":"paginated"}})
        ),
    )
    .unwrap();
    assert!(
        preserve_history_checks(&home, &vault)
            .await
            .unwrap_err()
            .to_string()
            .contains("PAGINATED_HISTORY_SERVER_REQUIRED")
    );
    let native = HistoryCheck {
        version: 1,
        thread_id: "test".into(),
        turns: 2,
        items: 5,
        digest: "native-page-fingerprint".into(),
    };
    let proof = vec![PreservedCheck {
        relative: relative.into(),
        digest: rollout_digest(&file).unwrap(),
        native: Some(native.clone()),
    }];
    atomic_write(
        &proof_path(&vault).unwrap(),
        &serde_json::to_vec(&proof).unwrap(),
    )
    .unwrap();
    // Explicit recovery reuses a previously verified proof without credentials
    // or another native server, even after the original metadata owner stopped.
    preserve_history_checks(&home, &vault).await.unwrap();
    fs::create_dir_all(vault.join("sessions")).unwrap();
    let saved = vault.join(relative);
    fs::rename(&file, &saved).unwrap();
    verify_preserved_history(&vault).unwrap();
    assert_eq!(
        preserved_checks(&vault, std::slice::from_ref(&saved)).unwrap(),
        vec![native]
    );
    fs::write(&saved, "changed").unwrap();
    assert!(verify_preserved_history(&vault).is_err());
    assert!(preserved_checks(&vault, &[saved]).is_err());
}

#[test]
fn history_normalization_only_excludes_item_envelope_timings() {
    let source = json!({"turnId":"turn-1","startedAtMs":null,"completedAtMs":null,
        "item":{"id":"item-1","type":"agentMessage","text":"original","startedAtMs":123}});
    let mut rebuilt = source.clone();
    rebuilt["startedAtMs"] = json!(1000);
    rebuilt["completedAtMs"] = json!(2000);
    assert_eq!(
        canonical_entry(&source, true, 1),
        canonical_entry(&rebuilt, true, 1)
    );
    assert_ne!(
        canonical_entry(&source, true, 0),
        canonical_entry(&rebuilt, true, 0)
    );
    assert_ne!(
        canonical_entry(&source, false, 1),
        canonical_entry(&rebuilt, false, 1)
    );
    for pointer in ["/turnId", "/item/id", "/item/text", "/item/startedAtMs"] {
        let mut changed = rebuilt.clone();
        *changed.pointer_mut(pointer).unwrap() = json!("changed");
        assert_ne!(
            canonical_entry(&source, true, 1),
            canonical_entry(&changed, true, 1)
        );
    }
    let legacy: HistoryCheck = serde_json::from_value(json!({
        "thread_id":"thread-1","turns":1,"items":1,"digest":"legacy"
    }))
    .unwrap();
    assert_eq!(legacy.version, 0);
}
