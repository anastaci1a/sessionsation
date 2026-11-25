use super::super::herdr_session_from_socket;
use std::path::Path;
#[test]
fn herdr_session_is_derived_only_from_the_socket_layout() {
    let s = |p: &str| herdr_session_from_socket(Path::new(p));
    assert_eq!(s("/data/herdr/herdr.sock").as_deref(), Some("default"));
    assert_eq!(
        s("/data/herdr/sessions/work-1/herdr.sock").as_deref(),
        Some("work-1")
    );
    assert_eq!(s("/data/herdr/other.sock"), None);
    assert_eq!(s("/data/herdr/sessions/bad name/herdr.sock"), None);
}
