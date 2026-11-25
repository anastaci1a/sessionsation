use super::*;

fn session(directory: &str) -> Session {
    serde_json::from_value(
        json!({"id":uuid::Uuid::new_v4(),"name":"n","provider":"claude",
        "account":"a","conversation_id":uuid::Uuid::new_v4(),"revision":1,"attachment":null,
        "directory":directory}),
    )
    .unwrap()
}

#[test]
fn restore_remaps_the_project_key_and_reads_the_manifest_through_the_alias() {
    let t = tempfile::tempdir().unwrap();
    let old_root = t.path().join("user/appdata/root");
    let new_root = t.path().join("disk/appdata/root");
    let old_home = old_root.join("accounts/claude/a");
    let new_home = new_root.join("accounts/claude/a");
    let old_dir = t.path().join("user/live/p").to_string_lossy().into_owned();
    let new_dir = t.path().join("disk/live/p").to_string_lossy().into_owned();
    let key = |d: &str| -> String {
        d.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    };
    let archived = session(&old_dir);
    let m = Manifest {
        version: 1,
        session: archived.clone(),
        provider: "claude".into(),
        home: old_home.clone(),
        state: "archived".into(),
        archived_at: 0,
        bundle: uuid::Uuid::new_v4().to_string(),
        conversation_ids: vec![archived.conversation_id.clone()],
        entries: vec![],
        nothing_to_archive: false,
    };
    let transcript = old_home
        .join("projects")
        .join(key(&old_dir))
        .join("x.jsonl");
    let history = old_home.join("file-history/x/h1");
    let other_key = old_home.join("projects/-elsewhere/x.jsonl");
    // Without an alias and without a directory change: exactly the original.
    assert_eq!(
        restore_target(&m, &archived, &old_home, &transcript).unwrap(),
        transcript
    );
    // Catalog directory re-keyed (archived_rekey_at_restore) and the alias active.
    let entries = [
        crate::alias::Entry {
            from: t.path().join("user/appdata"),
            to: t.path().join("disk/appdata"),
            proof: "test".into(),
        },
        crate::alias::Entry {
            from: t.path().join("user/live"),
            to: t.path().join("disk/live"),
            proof: "test".into(),
        },
    ];
    crate::alias::install(&entries);
    let mut current = archived.clone();
    current.directory = Some(new_dir.clone());
    assert_eq!(
        restore_target(&m, &current, &new_home, &transcript).unwrap(),
        new_home
            .join("projects")
            .join(key(&new_dir))
            .join("x.jsonl")
    );
    // Other entries keep their relative path (under the current home).
    assert_eq!(
        restore_target(&m, &current, &new_home, &history).unwrap(),
        new_home.join("file-history/x/h1")
    );
    assert_eq!(
        restore_target(&m, &current, &new_home, &other_key).unwrap(),
        new_home.join("projects/-elsewhere/x.jsonl")
    );
    crate::alias::uninstall(&entries);
}
