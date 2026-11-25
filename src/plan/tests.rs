use super::*;
#[test]
fn names_only_known_values_and_keeps_raw() {
    assert_eq!(name(ProviderId::Claude, "pro"), Some("Claude Pro"));
    // `auth status` reports "max" for both Max and Ultra: no name, raw kept.
    assert_eq!(name(ProviderId::Claude, "max"), None);
    assert_eq!(name(ProviderId::Codex, "prolite"), None);
    let o = PlanObservation {
        raw: "prolite".into(),
        rate_limit_tier: None,
        verified_at: 5,
    };
    let p = project(ProviderId::Codex, Some(&o), "usage_snapshot");
    assert_eq!(p["raw"], "prolite");
    assert!(p["name"].is_null() && p["rate_limit_tier"].is_null());
    assert_eq!(p["verified_at"], 5);
    assert!(project(ProviderId::Gemini, None, "x").is_null());
}
#[test]
fn claude_auth_carries_tier_only_when_reported() {
    let m = json!({"subscription_type":"max"});
    let o = from_claude_auth(&m, 9).unwrap();
    assert_eq!(
        (o.raw.as_str(), o.rate_limit_tier.clone(), o.verified_at),
        ("max", None, 9)
    );
    let m = json!({"subscription_type":"max","rate_limit_tier":"default_claude_max_5x"});
    assert_eq!(
        from_claude_auth(&m, 9).unwrap().rate_limit_tier.as_deref(),
        Some("default_claude_max_5x")
    );
    assert!(from_claude_auth(&json!({"subscription_type":null}), 9).is_none());
}
#[test]
fn codex_plan_uses_the_usage_observation_time() {
    let u = crate::model::Usage {
        plan_type: Some("plus".into()),
        observed_at: 100,
        ..Default::default()
    };
    assert_eq!(from_codex_usage(&u, 500).unwrap().verified_at, 100);
    assert_eq!(from_codex_usage(&u, 50).unwrap().verified_at, 50);
    assert!(from_codex_usage(&crate::model::Usage::default(), 5).is_none());
}
