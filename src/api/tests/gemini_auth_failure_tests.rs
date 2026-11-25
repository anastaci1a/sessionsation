use super::super::*;
use crate::provider::gemini::auth_failure;
#[test]
fn a_failure_recorded_after_the_cached_state_shows_at_once_and_clears() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let (home, _) = crate::provider::gemini::create_home(&paths, "g1").unwrap();
    std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
    // A cached state written before the failing run.
    let row = json!({"name":"g1","provider":"gemini","enabled":true,"auth_required":false,"deletion_id":null,"auth_detail":null});
    let mut cached = json!({"accounts": project_accounts(&paths, vec![row]).unwrap()});
    assert_eq!(cached["accounts"][0]["selectable"], true);
    auth_failure::record(
        &paths,
        "g1",
        &home,
        auth_failure::tests::UNSUPPORTED_CLIENT_STDERR,
    )
    .unwrap();
    refresh_cached_accounts(&paths, &mut cached);
    let a = &cached["accounts"][0];
    assert_eq!(a["auth_failed"], true);
    assert_eq!(a["auth_required"], true);
    assert_eq!(a["auth_detail"], "AUTH_FAILED");
    assert_eq!(a["selectable"], false);
    assert_eq!(a["prefill"]["eligible"], false);
    // A later success clears it in the cached rows too.
    auth_failure::clear(&paths, "g1");
    refresh_cached_accounts(&paths, &mut cached);
    let a = &cached["accounts"][0];
    assert_eq!(a["auth_failed"], false);
    assert_eq!(a["auth_required"], false);
    assert!(a["auth_detail"].is_null());
    assert_eq!(a["selectable"], true);
    // A Gemini login problem other than the failure is kept.
    let mut other = json!({"accounts":[{"name":"g1","provider":"gemini","enabled":true,"auth_required":true,"deletion_id":null,"auth_detail":"AUTH_REQUIRED","membership":"enrolled"}]});
    refresh_cached_accounts(&paths, &mut other);
    assert_eq!(other["accounts"][0]["auth_required"], true);
    assert_eq!(other["accounts"][0]["selectable"], false);
}
#[test]
fn recorded_gemini_auth_failure_makes_the_account_unselectable_until_cleared() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let (home, _) = crate::provider::gemini::create_home(&paths, "g1").unwrap();
    std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
    let row = || json!({"name":"g1","provider":"gemini","enabled":true,"auth_required":false,"deletion_id":null,"auth_detail":null});
    let project = || project_accounts(&paths, vec![row()]).unwrap().remove(0);
    let a = project();
    assert_eq!(a["auth_failed"], false);
    assert_eq!(a["selectable"], true);
    // A generic crash does not mark the account failed.
    auth_failure::record(&paths, "g1", &home, "panic: segmentation fault").unwrap();
    assert_eq!(project()["auth_failed"], false);
    auth_failure::record(&paths, "g1", &home, "Error: client no longer supported").unwrap();
    let a = project();
    assert_eq!(a["auth_failed"], true);
    assert!(
        a["auth_failure_reason"]
            .as_str()
            .unwrap()
            .starts_with("AUTH_CLIENT_UNSUPPORTED")
    );
    assert!(a["auth_failure_observed_at"].as_i64().is_some());
    assert_eq!(a["auth_required"], true);
    assert_eq!(a["selectable"], false);
    assert_eq!(a["prefill"]["eligible"], false);
    // Claude rows are untouched.
    let c = project_accounts(&paths, vec![json!({"name":"g1","provider":"claude","enabled":true,"auth_required":false,"deletion_id":null})]).unwrap().remove(0);
    assert!(c.get("auth_failed").is_none());
    auth_failure::clear(&paths, "g1");
    let a = project();
    assert_eq!(a["auth_failed"], false);
    assert_eq!(a["selectable"], true);
}
