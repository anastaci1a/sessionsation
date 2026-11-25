use super::*;
fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let home = paths.root.join("accounts/gemini/g1");
    fs::create_dir_all(home.join(".gemini")).unwrap();
    fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
    (t, paths, home)
}
#[test]
fn only_recognizable_auth_failures_classify() {
    for (text, code) in [
        (
            "Error: This client no longer supported. See issue",
            "AUTH_CLIENT_UNSUPPORTED",
        ),
        ("{\"error\":\"invalid_grant\"}", "AUTH_GRANT_INVALID"),
        (
            "GaxiosError: status: UNAUTHENTICATED",
            "AUTH_UNAUTHENTICATED",
        ),
        ("request failed with status 401", "AUTH_UNAUTHENTICATED"),
        ("Please re-authenticate with /auth", "AUTH_REAUTH_REQUIRED"),
        (
            r#"[API Error: {"error":{"code":400,"message":"API key not valid. Please pass a valid API key.","status":"INVALID_ARGUMENT","details":[{"reason":"API_KEY_INVALID"}]}}]"#,
            "AUTH_API_KEY_INVALID",
        ),
        (
            "When using Gemini API, you must specify the GEMINI_API_KEY environment variable.",
            "AUTH_API_KEY_MISSING",
        ),
    ] {
        assert_eq!(classify(text).unwrap().0, code, "{text}");
    }
    for text in [
        "",
        "segmentation fault",
        "ECONNRESET",
        "exit 1",
        "model overloaded 503",
    ] {
        assert!(classify(text).is_none(), "{text}");
    }
}
/// Real stderr of a headless `gemini -p "" -o json` run on an account whose
/// individual Google login was retired (gemini-cli 0.66.0, exit 1, empty stdout);
/// only the install path is redacted.
pub(crate) const UNSUPPORTED_CLIENT_STDERR: &str =
    include_str!("../headless_unsupported_client.stderr");
#[test]
fn real_unsupported_client_output_is_a_sign_in_failure() {
    let c = classify_output(UNSUPPORTED_CLIENT_STDERR, Some(1)).unwrap();
    assert_eq!(c.code, "AUTH_CLIENT_UNSUPPORTED");
    assert!(c.at_sign_in);
    assert!(c.reason.len() <= MAX_REASON);
    // Each recognizable line on its own still classifies.
    for line in [
        "Error authenticating: IneligibleTierError: something new",
        "reasonCode: 'UNSUPPORTED_CLIENT',",
        "To continue using Gemini, please migrate to the Antigravity suite of products",
        "This client is no longer supported for Gemini Code Assist for individuals.",
    ] {
        let c = classify_output(line, Some(1)).unwrap();
        assert!(c.at_sign_in, "{line}");
    }
    // The CLI's fatal-authentication exit status counts without known text, and a
    // JSON error naming the fatal authentication error does too.
    let c = classify_output("", Some(EXIT_AUTHENTICATION)).unwrap();
    assert_eq!((c.code, c.at_sign_in), ("AUTH_FAILED", true));
    let c = classify_output(
        r#"{"type":"FatalAuthenticationError","message":"x","code":41}"#,
        Some(1),
    )
    .unwrap();
    assert_eq!((c.code, c.at_sign_in), ("AUTH_FAILED", true));
    // Authorization refusals are recognized; a mid-run 401 is not a sign-in failure.
    assert_eq!(
        classify_output("status: PERMISSION_DENIED", Some(1))
            .unwrap()
            .code,
        "AUTH_PERMISSION_DENIED"
    );
    assert!(
        !classify_output("request failed with status 401", Some(1))
            .unwrap()
            .at_sign_in
    );
    // A network failure while signing in says nothing about the credentials.
    assert!(classify_output("Error authenticating: FetchError: ECONNRESET", Some(1)).is_none());
    assert!(classify_output("Error authenticating: socket hang up", Some(1)).is_none());
    assert!(classify_output("model overloaded 503", Some(1)).is_none());
}
#[test]
fn records_clears_and_is_superseded_by_new_credentials() {
    let (_t, paths, home) = setup();
    assert!(record(&paths, "g1", &home, "ECONNRESET").unwrap().is_none());
    assert!(current(&paths, "g1", &home).is_none());
    let secret = "ya29.SECRET-TOKEN-VALUE";
    let f = record(&paths, "g1", &home, &format!("invalid_grant {secret}"))
        .unwrap()
        .unwrap();
    assert_eq!(f.code, "AUTH_GRANT_INVALID");
    let stored = fs::read_to_string(file(&paths, "g1").unwrap()).unwrap();
    assert!(!stored.contains(secret), "raw output is never stored");
    assert!(stored.len() < 1024);
    assert_eq!(current(&paths, "g1", &home), Some(f));
    // A newer credential file supersedes the failure.
    fs::write(home.join(".gemini/oauth_creds.json"), b"{\"a\":1}").unwrap();
    assert!(current(&paths, "g1", &home).is_none());
    record(&paths, "g1", &home, "invalid_grant").unwrap();
    assert!(current(&paths, "g1", &home).is_some());
    clear(&paths, "g1");
    assert!(current(&paths, "g1", &home).is_none());
    assert_eq!(fields(None)["auth_failed"], false);
}
