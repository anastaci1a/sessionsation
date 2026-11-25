use super::super::*;
#[test]
fn recovery_cannot_clear_a_newer_stop_intent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    let id = uuid::Uuid::new_v4().to_string();
    set_stop_intent(&paths, &id, Some("new-stop".into())).unwrap();
    assert!(clear_stop_intent(&paths, &id, Some("old-stop")).is_err());
    assert_eq!(
        stop_intent(&paths, &id).unwrap().as_deref(),
        Some("new-stop")
    );
    assert!(clear_stop_intent(&paths, &id, None).is_err());
    clear_stop_intent(&paths, &id, Some("new-stop")).unwrap();
    assert!(stop_intent(&paths, &id).unwrap().is_none());
}
