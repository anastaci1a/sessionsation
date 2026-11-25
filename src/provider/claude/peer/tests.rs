use super::*;
#[test]
fn unlinked_socket_has_a_distinct_reason() {
    let d = tempfile::tempdir().unwrap();
    let missing = d.path().join("cc-socks/1.sock");
    let r = unlinked_reason(std::process::id(), &missing);
    assert!(r.starts_with("PEER_SOCKET_UNLINKED"), "{r}");
    assert!(!r.contains("still listens"));
    let sock = d.path().join("l.sock");
    let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    fs::remove_file(&sock).unwrap();
    let r = unlinked_reason(std::process::id(), &sock);
    assert!(r.starts_with("PEER_SOCKET_UNLINKED"), "{r}");
    assert!(r.contains("still listens"), "{r}");
}

#[test]
fn toggle_preserves_other_keys_and_operator_values() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path();
    // Absent file: disable is a no-op, enable creates it.
    assert!(!set(home, false).unwrap().changed);
    assert!(!settings_file(home).exists());
    let t = set(home, true).unwrap();
    assert!(t.changed && t.previous.is_none());
    assert!(enabled(home));
    fs::write(
        settings_file(home),
        r#"{"theme":"dark","permissions":{"allow":["Bash(ls)"]},"crossSessionInbound":"accept"}"#,
    )
    .unwrap();
    let t = set(home, false).unwrap();
    assert!(t.changed);
    let v: Value = serde_json::from_slice(&fs::read(settings_file(home)).unwrap()).unwrap();
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
    assert!(v.get(SETTING).is_none());
    assert!(!set(home, false).unwrap().changed, "idempotent");
    // An operator's own hold is never removed by disable.
    fs::write(settings_file(home), r#"{"crossSessionInbound":"hold"}"#).unwrap();
    let t = set(home, false).unwrap();
    assert!(!t.changed);
    assert_eq!(inbound(home).as_deref(), Some("hold"));
    assert!(!enabled(home));
    fs::write(settings_file(home), "[1]").unwrap();
    assert!(
        set(home, true)
            .unwrap_err()
            .to_string()
            .contains("SETTINGS_INVALID")
    );
}
