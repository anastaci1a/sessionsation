use super::notice_kind;

#[test]
fn an_exit_notice_is_an_exited_event() {
    assert_eq!(notice_kind("exited"), "exited");
    assert_eq!(notice_kind("idle"), "idle");
    assert_eq!(notice_kind("anything-else"), "idle");
}
