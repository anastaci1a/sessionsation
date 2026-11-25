use super::super::*;
#[test]
fn reserved_names_are_unique_and_convert_without_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
    reserve_name(&paths, None, "chosen", "request-a").unwrap();
    assert!(reserve_name(&paths, None, "chosen", "request-b").is_err());
    let other = record_launch(&paths, "account", "conversation-b", "chosen", None).unwrap();
    assert_eq!(other.name, "chosen-2");
    assert!(rename(&paths, &other.id, &other.generation(), "chosen").is_err());
    let s = complete_reserved(
        &paths,
        "request-a",
        None,
        "chosen",
        ProviderId::Codex,
        "account",
        "conversation-a",
        None,
        None,
    )
    .unwrap();
    assert_eq!(s.id, "request-a");
    assert_eq!(s.name, "chosen");
    assert_eq!(s.account, "account");
    assert!(check_name(&paths, None, "chosen", "request-a").is_err());
}
