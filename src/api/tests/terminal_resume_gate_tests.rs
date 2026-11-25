use super::super::ensure_resumable_in_terminal as gate;

#[test]
fn unfinished_history_needs_explicit_confirmation_and_nothing_else_passes() {
    assert!(gate(None, false).is_ok());
    let e = gate(Some("unfinished"), false).unwrap_err().to_string();
    assert!(
        e.contains("UNFINISHED_HISTORY_CONFIRMATION_REQUIRED"),
        "{e}"
    );
    assert!(gate(Some("unfinished"), true).is_ok());
    for reason in ["busy", "unknown", "diverged", "moving", "future"] {
        let e = gate(Some(reason), true).unwrap_err().to_string();
        assert!(e.contains("SESSION_STATE_UNVERIFIED"), "{e}");
    }
}
