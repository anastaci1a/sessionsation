use super::*;

#[test]
fn strips_control_sequences() {
    let t = strip_ansi(b"\x1b[2J\x1b[1;1Hhi \x1b]8;;x\x07there\x1b[0m");
    assert_eq!(t, "hi there");
}

#[test]
fn rejoins_a_hard_wrapped_url_and_stops_at_prose() {
    let text = "Visit:\r\nhttps://accounts.google.com/o/oauth2/v2/auth?client_id=abc&\r\nredirect_uri=https%3A%2F%2Fcodeassist.google.com&state=xyz\r\n\r\nEnter the code:";
    let url = find_auth_url(text, 62).unwrap();
    assert_eq!(
        url,
        "https://accounts.google.com/o/oauth2/v2/auth?client_id=abc&redirect_uri=https%3A%2F%2Fcodeassist.google.com&state=xyz"
    );
}

#[test]
fn rejoins_an_indented_url_box_like_antigravity_prints() {
    // agy 1.4.10: " Open the URL below", a rule, then the URL indented by one
    // space and wrapped with the same indent (synthetic values).
    let text = " Open the URL below in your browser:\r\n ────────────────────\r\n https://accounts.google.com/o/oauth2/auth?access_type=off\r\n line&client_id=fake.apps.googleusercontent.com&state=abcd\r\n ef\r\n ────────────────────\r\n";
    let url = find_auth_url(text, 58).unwrap();
    assert_eq!(
        url,
        "https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=fake.apps.googleusercontent.com&state=abcdef"
    );
}

#[test]
fn does_not_join_a_short_line_with_the_next() {
    let url = find_auth_url("https://example.invalid/a\nnextword", 80).unwrap();
    assert_eq!(url, "https://example.invalid/a");
    assert!(find_auth_url("no link here", 80).is_none());
}

#[test]
fn status_row_links_and_copies_without_the_url_as_visible_text() {
    let row = status_row(Some("https://example.invalid/x"), 24, true);
    assert!(row.contains("\x1b]8;;https://example.invalid/x\x07"));
    assert!(row.contains("\x1b]52;c;aHR0cHM6Ly9leGFtcGxlLmludmFsaWQveA==\x07"));
    assert!(row.contains("Open the sign-in link"));
    assert!(!status_row(Some("https://example.invalid/x"), 24, false).contains("]52"));
    assert!(status_row(None, 24, true).contains("Waiting"));
}

#[test]
fn base64_pads() {
    assert_eq!(base64(b"a"), "YQ==");
    assert_eq!(base64(b"ab"), "YWI=");
    assert_eq!(base64(b"abc"), "YWJj");
}

#[test]
fn supervise_stops_a_process_that_keeps_running_once_ready() {
    // Needs a controlling terminal for stdin; skipped when none exists.
    if unsafe { libc::isatty(0) } != 1 {
        return;
    }
    let mut c = std::process::Command::new("sleep");
    c.arg("30");
    let start = Instant::now();
    supervise(c, || true).unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
}
