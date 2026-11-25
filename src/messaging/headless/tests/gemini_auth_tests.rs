use super::super::*;
use crate::provider::gemini::auth_failure as failure;
#[test]
fn headless_gemini_runs_record_only_recognizable_auth_failures_and_clear_on_success() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let home = paths.root.join("accounts/gemini/g1");
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
    let cur = || failure::current(&paths, "g1", &home);
    // Generic crash: nothing recorded.
    observe_gemini_auth(&paths, "g1", &home, b"", b"Segmentation fault", Some(139));
    assert!(cur().is_none());
    // Auth rejection on stderr of a failed run.
    observe_gemini_auth(&paths, "g1", &home, b"", b"Error: invalid_grant", Some(41));
    assert_eq!(cur().unwrap().code, "AUTH_GRANT_INVALID");
    // Another crash neither clears nor replaces it.
    observe_gemini_auth(&paths, "g1", &home, b"", b"ECONNRESET", Some(1));
    assert!(cur().is_some());
    // A reply that merely mentions an auth term does not count as a failure and,
    // being a clean success, clears the record.
    let ok = br#"{"session_id":"s","response":"UNAUTHENTICATED means 401","stats":{}}"#;
    observe_gemini_auth(&paths, "g1", &home, ok, b"", Some(0));
    assert!(cur().is_none());
    // A JSON-reported error is classified from the error object.
    let bad =
        br#"{"session_id":"s","error":{"type":"Error","message":"client no longer supported"}}"#;
    observe_gemini_auth(&paths, "g1", &home, bad, b"", Some(1));
    assert_eq!(cur().unwrap().code, "AUTH_CLIENT_UNSUPPORTED");
    // stderr of a successful run is never inspected.
    observe_gemini_auth(&paths, "g1", &home, ok, b"invalid_grant noise", Some(0));
    assert!(cur().is_none());
}
