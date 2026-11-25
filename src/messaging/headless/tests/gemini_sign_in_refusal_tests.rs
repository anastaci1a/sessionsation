use super::super::*;
use crate::provider::gemini::auth_failure as failure;
#[test]
fn real_unsupported_client_run_is_a_definite_refusal_and_marks_the_account() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let home = paths.root.join("accounts/gemini/g1");
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
    let stderr = failure::tests::UNSUPPORTED_CLIENT_STDERR.as_bytes();
    // The real run: exit 1, empty stdout, the tier rejection on stderr.
    assert!(parse(ProviderId::Gemini, b"", Some(1)).is_none());
    let auth = observe_gemini_auth(&paths, "g1", &home, b"", stderr, Some(1)).unwrap();
    assert!(auth.at_sign_in);
    let recorded = failure::current(&paths, "g1", &home).unwrap();
    assert_eq!(recorded.code, "AUTH_CLIENT_UNSUPPORTED");
    // Untouched conversation: a definite refusal naming the reason.
    let reason = sign_in_refusal(Some(&auth), &None::<u8>, &None).unwrap();
    assert!(reason.starts_with("AUTH_CLIENT_UNSUPPORTED:"), "{reason}");
    assert!(reason.ends_with("nothing was sent to the model"));
    // A conversation file that changed during the run keeps it uncertain.
    assert!(sign_in_refusal(Some(&auth), &None, &Some(1u8)).is_none());
    // A failure that is not a sign-in failure (a 401 mid-run) stays uncertain,
    // and so does a run with no recognized authentication failure.
    let mid = observe_gemini_auth(&paths, "g1", &home, b"", b"status 401", Some(1)).unwrap();
    assert!(!mid.at_sign_in);
    assert!(sign_in_refusal(Some(&mid), &None::<u8>, &None).is_none());
    assert!(sign_in_refusal(None, &None::<u8>, &None).is_none());
    // A JSON-reported fatal authentication exit code counts like the status.
    let json = br#"{"session_id":"s","error":{"type":"Error","message":"x","code":41}}"#;
    assert!(
        observe_gemini_auth(&paths, "g1", &home, json, b"", Some(1))
            .unwrap()
            .at_sign_in
    );
    // A later clean run clears the mark.
    let ok = br#"{"session_id":"s","response":"hi","stats":{}}"#;
    assert!(observe_gemini_auth(&paths, "g1", &home, ok, b"", Some(0)).is_none());
    assert!(failure::current(&paths, "g1", &home).is_none());
}
