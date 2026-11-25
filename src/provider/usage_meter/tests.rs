use super::*;

#[test]
fn every_provider_has_the_same_meter_keys_and_nothing_is_invented() {
    let now = 1_000_000;
    let codex = json!({"short":{"used_percent":23.0,"duration_minutes":300,"resets_at":now+100},
        "weekly":{"used_percent":42.0,"duration_minutes":10080,"resets_at":now+1000},"observed_at":now-10});
    let claude = json!({"short":{"used_percent":31.0,"resets_at":now+100,"observed_at":now-50,"source":"oauth"},
        "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-5,"source":"statusline"},
        "observed_at":now-5});
    let keys = |v: &Value| {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    let rows = [
        of(ProviderId::Codex, &codex, now, 300),
        of(ProviderId::Claude, &claude, now, 300),
        of(ProviderId::Gemini, &json!({}), now, 300),
        of(ProviderId::Antigravity, &json!({}), now, 300),
        of(
            ProviderId::Claude,
            &json!({"short":null,"weekly":null,"observed_at":null}),
            now,
            300,
        ),
    ];
    for r in &rows {
        assert_eq!(keys(r), keys(&rows[0]));
    }
    let c = &rows[0];
    assert_eq!(c["source"], "app_server");
    assert_eq!(c["requires_session"], false);
    assert_eq!(c["fresh"], true);
    assert_eq!(c["age_seconds"], 10);
    assert_eq!(c["windows"]["five_hour"]["used_percent"], 23.0);
    let c = &rows[1];
    assert_eq!(c["source"], "statusline", "the newest window's source");
    assert_eq!(c["observed_at"], now - 5);
    assert_eq!(c["windows"]["five_hour"]["source"], "oauth");
    assert_eq!(rows[2]["supported"], false);
    assert!(rows[2]["reason"].is_string());
    assert_eq!(rows[4]["fresh"], false);
    assert!(rows[4]["windows"]["five_hour"].is_null());
    assert!(rows[4]["source"].is_null());
    // Past a reset, or too old: not fresh.
    assert_eq!(
        of(ProviderId::Codex, &codex, now + 200, 300)["fresh"],
        false
    );
    assert_eq!(of(ProviderId::Codex, &codex, now + 50, 30)["fresh"], false);
}

#[test]
fn each_window_has_its_own_freshness_and_age_from_its_own_observed_at() {
    // Fake clock: Claude's five_hour is from the statusline just now, seven_day from an
    // OAuth read 256 s earlier (the live lag): same bound, different ages.
    let now = 1_000_000;
    let usage = json!({"short":{"used_percent":31.0,"resets_at":now+100,"observed_at":now-3,"source":"statusline"},
        "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-256,"source":"oauth"},
        "observed_at":now-3});
    let m = of(ProviderId::Claude, &usage, now, 300);
    let (five, seven) = (&m["windows"]["five_hour"], &m["windows"]["seven_day"]);
    assert_eq!(
        (five["age_seconds"].as_i64(), seven["age_seconds"].as_i64()),
        (Some(3), Some(256))
    );
    assert_eq!(
        (five["fresh"].as_bool(), seven["fresh"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(
        (m["fresh"].as_bool(), m["all_windows_fresh"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(m["age_seconds"], 3, "meter.fresh stays the newest reading");
    // The weekly read ages past the bound while the statusline keeps the meter fresh.
    let later = now + claude::USAGE_STALE_SECONDS;
    let usage = json!({"short":{"used_percent":31.0,"resets_at":later+100,"observed_at":later-3,"source":"statusline"},
        "weekly":{"used_percent":56.5,"resets_at":later+1000,"observed_at":now-256,"source":"oauth"},
        "observed_at":later-3});
    let m = of(ProviderId::Claude, &usage, later, 300);
    assert_eq!(m["windows"]["five_hour"]["fresh"], true);
    assert_eq!(m["windows"]["seven_day"]["fresh"], false);
    assert_eq!(
        m["windows"]["seven_day"]["age_seconds"],
        claude::USAGE_STALE_SECONDS + 256
    );
    assert_eq!(
        (m["fresh"].as_bool(), m["all_windows_fresh"].as_bool()),
        (Some(true), Some(false))
    );
    // A window past its own reset is not fresh and says why; the other one is unaffected.
    let usage = json!({"short":{"used_percent":31.0,"resets_at":now-1,"observed_at":now-3,"source":"statusline"},
        "weekly":{"used_percent":56.5,"resets_at":now+1000,"observed_at":now-4,"source":"oauth"},
        "observed_at":now-3});
    let m = of(ProviderId::Claude, &usage, now, 300);
    assert_eq!(m["windows"]["five_hour"]["fresh"], false);
    assert_eq!(m["windows"]["seven_day"]["fresh"], true);
    assert_eq!(m["window_reasons"]["five_hour"], reset_reason(now - 1));
    assert_eq!(m["all_windows_fresh"], false);
    // Unsupported and absent windows are never fresh.
    assert_eq!(
        of(ProviderId::Gemini, &json!({}), now, 300)["all_windows_fresh"],
        false
    );
    let none = of(
        ProviderId::Claude,
        &json!({"short":null,"weekly":null}),
        now,
        300,
    );
    assert_eq!(none["all_windows_fresh"], false);
    assert!(none["windows"]["five_hour"].is_null());
}
