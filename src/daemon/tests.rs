#[test]
fn a_stale_codex_meter_always_says_why_and_a_fresh_one_never_does() {
    use crate::provider::usage_meter;
    let now = 1_000_000;
    let old = json!({"short":{"used_percent":23.0,"duration_minutes":300,"resets_at":now+100},
        "weekly":{"used_percent":42.0,"duration_minutes":10080,"resets_at":now+1000},"observed_at":now-10_000});
    let mut m = usage_meter::of(ProviderId::Codex, &old, now, 300);
    assert_eq!(m["fresh"], false);
    super::explain_stale_meter(&mut m, None);
    assert!(m["reason"].as_str().unwrap().contains("10000 s old"), "{m}");
    let mut m = usage_meter::of(ProviderId::Codex, &old, now, 300);
    super::explain_stale_meter(&mut m, Some("lease".into()));
    assert_eq!(m["reason"], "lease");
    let mut never = usage_meter::of(ProviderId::Codex, &json!({}), now, 300);
    super::explain_stale_meter(&mut never, Some("lease".into()));
    assert_eq!(never["reason"], "no usage window has been read yet; lease");
    let young = json!({"short":{"used_percent":23.0,"duration_minutes":300,"resets_at":now+100},
        "weekly":{"used_percent":42.0,"duration_minutes":10080,"resets_at":now+1000},"observed_at":now-10});
    let mut fresh = usage_meter::of(ProviderId::Codex, &young, now, 300);
    super::explain_stale_meter(&mut fresh, Some("lease".into()));
    assert_eq!(fresh["fresh"], true);
    assert!(fresh["reason"].is_null(), "{fresh}");
}
#[test]
fn account_rows_carry_the_observed_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let mut state = State::default();
    let mut cx = Account::new("cx".into(), "fixture".into());
    cx.usage.plan_type = Some("prolite".into());
    cx.usage.observed_at = 1234;
    state.accounts.insert("cx".into(), cx);
    let mut cl = Account::with_provider(ProviderId::Claude, "cl".into(), String::new());
    cl.plan = Some(crate::plan::PlanObservation {
        raw: "max".into(),
        rate_limit_tier: None,
        verified_at: 77,
    });
    state.insert_account(cl);
    state.insert_account(Account::with_provider(
        ProviderId::Gemini,
        "gm".into(),
        String::new(),
    ));
    let v = status(&paths, &state, &Config::default(), "fixture");
    let plan = |n: &str| {
        v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == n)
            .unwrap()["plan"]
            .clone()
    };
    assert_eq!(
        plan("cx"),
        json!({"provider":"codex","raw":"prolite","name":null,"rate_limit_tier":null,"source":"usage_snapshot","verified_at":1234})
    );
    assert_eq!(
        plan("cl"),
        json!({"provider":"claude","raw":"max","name":null,"rate_limit_tier":null,"source":"auth_status","verified_at":77})
    );
    assert!(plan("gm").is_null());
    let mut p = Some(crate::plan::PlanObservation {
        raw: "max".into(),
        rate_limit_tier: None,
        verified_at: 10,
    });
    let same = |at| crate::plan::PlanObservation {
        raw: "max".into(),
        rate_limit_tier: None,
        verified_at: at,
    };
    assert!(!record_plan(&mut p, &same(20)) && p.as_ref().unwrap().verified_at == 20);
    assert!(record_plan(&mut p, &same(20 + 3600)));
    assert!(record_plan(
        &mut p,
        &crate::plan::PlanObservation {
            raw: "pro".into(),
            ..same(20 + 3601)
        }
    ));
}
use super::*;
#[test]
fn usage_carries_reset_credits_only_when_reported() {
    let limits = json!({"limitId":"codex","primary":{"usedPercent":5,"windowDurationMins":300,"resetsAt":10}});
    let plain = parse_usage(&json!({"rateLimits":limits}), 3).unwrap();
    assert!(plain.reset_credits.is_none());
    let mut a = Account::new("cx".into(), "fixture".into());
    a.usage = plain;
    let paths = Paths::new(Some(tempfile::tempdir().unwrap().keep().join("s"))).unwrap();
    let (usage, _, _) = account_usage(&paths, &a, &Config::default(), 5);
    assert!(usage["reset_credits"].is_null());
    let with = parse_usage(
        &json!({"rateLimits":limits,"rateLimitResetCredits":{"availableCount":4}}),
        3,
    )
    .unwrap();
    a.usage = with;
    let (usage, _, _) = account_usage(&paths, &a, &Config::default(), 5);
    assert_eq!(
        usage["reset_credits"],
        json!({"available":4,"observed_at":3,"credits":null,"fresh":true,"age_seconds":2})
    );
    // Contract 1.65.0: the age is the meter's rule (Codex `stale_seconds`).
    let stale = Config::default().stale_seconds as i64;
    let (usage, _, _) = account_usage(&paths, &a, &Config::default(), 3 + stale);
    assert_eq!(usage["reset_credits"]["fresh"], false);
    assert_eq!(usage["reset_credits"]["age_seconds"], stale);
    // Older stored usage without the field still loads.
    let old: Usage = serde_json::from_value(
        json!({"short":null,"weekly":null,"observed_at":1,"exhausted":false}),
    )
    .unwrap();
    assert!(old.reset_credits.is_none());
}
#[test]
fn unobserved_usage_reports_null_observed_at_never_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let config = Config::default();
    let mut state = State::default();
    state
        .accounts
        .insert("cx".into(), Account::new("cx".into(), "fixture".into()));
    state
        .accounts
        .insert("real".into(), Account::new("real".into(), "fixture".into()));
    state.accounts.get_mut("real").unwrap().usage.observed_at = 1234;
    state.accounts.insert(
        "gm".into(),
        Account::with_provider(ProviderId::Gemini, "gm".into(), String::new()),
    );
    state.accounts.insert(
        "cl".into(),
        Account::with_provider(ProviderId::Claude, "cl".into(), String::new()),
    );
    let report = status(&paths, &state, &config, "fixture");
    for a in report["accounts"].as_array().unwrap() {
        let observed = &a["usage"]["observed_at"];
        match a["name"].as_str().unwrap() {
            "real" => assert_eq!(observed, 1234),
            name => assert!(observed.is_null(), "{name}: {observed}"),
        }
    }
}

#[test]
fn a_future_observed_at_neither_blocks_new_polls_nor_is_reported() {
    let mut a = Account::new("cx".into(), "fixture".into());
    let ahead = now() + 3600;
    a.usage.observed_at = ahead;
    let fresh = Usage {
        observed_at: now(),
        ..Default::default()
    };
    a.observe(fresh.clone());
    assert_eq!(a.usage.observed_at, fresh.observed_at);
    // A genuinely older reading is still dropped.
    a.observe(Usage {
        observed_at: fresh.observed_at - 10,
        ..Default::default()
    });
    assert_eq!(a.usage.observed_at, fresh.observed_at);
    // Reporting clamps a value that is ahead of the clock.
    a.usage.observed_at = ahead;
    let paths = Paths::new(Some(tempfile::tempdir().unwrap().keep().join("s"))).unwrap();
    let (usage, fresh, _) = account_usage(&paths, &a, &Config::default(), now());
    assert!(usage["observed_at"].as_i64().unwrap() <= now());
    assert!(!fresh);
}

#[test]
fn locked_account_reports_paused_monitoring_without_claiming_fresh_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let home = paths.account("personal").unwrap();
    private_dir(&home).unwrap();
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(".switcher-lease"))
        .unwrap();
    lease.try_lock_exclusive().unwrap();
    let mut state = State::default();
    state.accounts.insert(
        "personal".into(),
        Account::new("personal".into(), "fixture".into()),
    );
    let report = status(&paths, &state, &Config::default(), "fixture");
    assert_eq!(
        report["accounts"][0]["monitoring_pause_reason"],
        "account_lease"
    );
    assert_eq!(report["accounts"][0]["usage_fresh"], false);
    FileExt::unlock(&lease).unwrap();
}
