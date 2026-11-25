use super::super::*;
#[test]
fn empty_admission_is_not_a_running_receipt_or_a_valid_inventory() {
    let receipt = decode_reply(&["pane", "run", "w1:p1", "command"], b"").unwrap();
    assert_eq!(receipt["admitted"], true);
    assert!(receipt.get("running").is_none());
    assert!(decode_reply(&["api", "snapshot"], b"").is_err());
    assert!(decode_reply(&["pane", "run"], br#"{"error":{"code":"no_pane"}}"#).is_err());
}
#[test]
fn retained_shell_is_not_an_agent_and_exec_is_not_an_idle_shell() {
    assert_eq!(
        process_state(&json!({"shell_pid":42,"foreground_processes":[{"pid":42,"name":"bash"}]}))
            .unwrap(),
        (true, 42)
    );
    assert_eq!(
        process_state(&json!({"shell_pid":42,"foreground_processes":[{"pid":42,"name":"codex"}]}))
            .unwrap(),
        (false, 42)
    );
    assert_eq!(
        process_state(&json!({"shell_pid":42,"foreground_processes":[{"pid":43,"name":"vim"}]}))
            .unwrap(),
        (false, 43)
    );
    assert!(process_state(&json!({"shell_pid":42,"foreground_processes":[]})).is_err());
}
#[test]
fn herdr_targets_are_scoped_and_validate_each_component() {
    assert_eq!(herdr_target("test", "w12:p3").unwrap(), "herdr/test/w12:p3");
    assert_eq!(
        herdr_target("default", "wA:pB").unwrap(),
        "herdr/default/wA:pB"
    );
    for (s, p) in [
        ("../default", "w1:p1"),
        ("default", "w:p1"),
        ("default", "w1:p1/extra"),
        ("default", "%1"),
    ] {
        assert!(herdr_target(s, p).is_err());
    }
}
