use super::super::*;
#[test]
fn verified_move_preserves_rename_is_idempotent_and_rejects_account_replacement() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let old = Attachment {
        host: "herdr".into(),
        target: "herdr/fixture/w1:p1".into(),
        pid: 11,
        process_start: 22,
    };
    let before = record_launch(
        &p,
        "example",
        &uuid::Uuid::new_v4().to_string(),
        "original",
        Some(old.clone()),
    )
    .unwrap();
    rename(&p, &before.id, &before.generation(), "renamed").unwrap();
    let new = Attachment {
        target: "herdr/fixture/w2:p2".into(),
        ..old
    };
    let moved = move_attachment(&p, &before, Some(new.clone()), None).unwrap();
    assert_eq!(moved.name, "renamed");
    assert_eq!(moved.conversation_id, before.conversation_id);
    assert_eq!(
        move_attachment(&p, &before, Some(new.clone()), None)
            .unwrap()
            .generation(),
        moved.generation()
    );
    record_launch(
        &p,
        "other",
        &before.conversation_id,
        "other",
        Some(new.clone()),
    )
    .unwrap();
    assert!(
        move_attachment(&p, &before, Some(new), None)
            .unwrap_err()
            .to_string()
            .contains("STALE_TARGET")
    );
}
