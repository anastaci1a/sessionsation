use super::super::*;
#[tokio::test]
async fn probe_reports_a_recorded_gemini_auth_failure_despite_present_credentials() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let home = paths.root.join("accounts/gemini/g1");
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::write(home.join(".gemini/oauth_creds.json"), b"{}").unwrap();
    std::fs::write(
        home.join(".gemini/google_accounts.json"),
        br#"{"active":"a@example.invalid"}"#,
    )
    .unwrap();
    let config = Config::default();
    let (problem, _) = probe(&paths, "g1", ProviderId::Gemini, &config, &home).await;
    assert_eq!(problem, None);
    gemini::auth_failure::record(&paths, "g1", &home, "invalid_grant").unwrap();
    let (problem, _) = probe(&paths, "g1", ProviderId::Gemini, &config, &home).await;
    assert_eq!(problem.as_deref(), Some("AUTH_FAILED"));
    gemini::auth_failure::clear(&paths, "g1");
    assert_eq!(
        probe(&paths, "g1", ProviderId::Gemini, &config, &home)
            .await
            .0,
        None
    );
}
