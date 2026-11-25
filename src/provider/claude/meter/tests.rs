use super::*;

#[test]
fn usage_answer_windows_become_oauth_readings_that_never_lower_a_cycle() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let now = 1_800_000_000;
    // 2027-01-15T08:00:00Z and 2027-01-20T08:00:00Z, with a fraction and an offset.
    let (r5, r7) = (1_800_000_000 + 3_600, 1_800_000_000 + 86_400 * 5);
    let iso = |t: i64, frac: &str| {
        let days = t / 86_400;
        let secs = t % 86_400;
        // Civil date from days (Howard Hinnant), as the parser's inverse.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}+00:00",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60
        )
    };
    let body = |five: f64, week: f64| {
        json!({"five_hour":{"utilization":five,"resets_at":iso(r5, ".482910")},
            "seven_day":{"utilization":week,"resets_at":iso(r7, "")},
            "seven_day_opus":null,"extra_usage":{"is_enabled":false}})
    };
    assert!(record_answer(home, &body(31., 55.), now).unwrap());
    let (usage, fresh, _) = super::super::project_usage(home, now + 1);
    assert!(fresh);
    assert_eq!(usage["short"]["used_percent"], 31.0);
    assert_eq!(usage["short"]["resets_at"], r5);
    assert_eq!(usage["short"]["source"], "oauth");
    assert_eq!(usage["weekly"]["used_percent"], 55.0);
    assert_eq!(usage["weekly"]["resets_at"], r7);
    // A statusline replay of the same cycle with less usage never lowers it, even with
    // the header's whole-second reset time.
    let replay = json!({"session_id":"s","version":"2.1.314","rate_limits":{
        "five_hour":{"used_percentage":20.0,"resets_at":r5},
        "seven_day":{"used_percentage":56.5,"resets_at":r7 + 1}}});
    super::super::record_statusline(home, &replay, now + 60).unwrap();
    let (usage, _, _) = super::super::project_usage(home, now + 61);
    assert_eq!(usage["short"]["used_percent"], 31.0);
    assert_eq!(usage["short"]["source"], "oauth");
    assert_eq!(usage["short"]["observed_at"], now);
    assert_eq!(
        usage["weekly"]["used_percent"], 56.5,
        "a higher reading wins"
    );
    assert_eq!(usage["weekly"]["source"], "statusline");
    assert_eq!(usage["weekly"]["observed_at"], now + 60);
    // And an older OAuth answer never lowers what the statusline reported.
    assert!(!record_answer(home, &body(30., 50.), now + 120).unwrap());
    let (usage, _, _) = super::super::project_usage(home, now + 121);
    assert_eq!(usage["weekly"]["used_percent"], 56.5);
    assert_eq!(usage["short"]["used_percent"], 31.0);
    // A new cycle (a reset hours later) replaces with a lower value.
    let next = json!({"five_hour":{"utilization":2.0,"resets_at":iso(r5 + 18_000, "")}});
    assert!(record_answer(home, &next, now + 3_700).unwrap());
    let (usage, _, _) = super::super::project_usage(home, now + 3_701);
    assert_eq!(usage["short"]["used_percent"], 2.0);
    assert_eq!(usage["weekly"]["used_percent"], 56.5, "carried forward");
    // Null utilization or reset time is nothing observed; over 100% is at the limit.
    let w = windows(
        &json!({"five_hour":{"utilization":null,"resets_at":iso(r5, "")},
        "seven_day":{"utilization":104.0,"resets_at":iso(r7, "")}}),
    );
    assert!(w["five_hour"].is_null());
    assert_eq!(w["seven_day"]["used_percentage"], 100.0);
    assert!(!record_answer(home, &json!({"extra_usage":null}), now).unwrap());
    assert!(record_answer(home, &json!([]), now).is_err());
}

/// Contract 1.65.0: a reset no read saw heals. One lower OAuth answer in a closed cycle can
/// be stale and is kept out; a second one at least CONFIRM_SECONDS later with the same or
/// less is accepted as a `provider_lowered` cycle.
#[test]
fn a_confirmed_lower_oauth_reading_heals_a_missed_reset_and_a_single_one_does_not() {
    let t = tempfile::tempdir().unwrap();
    let home = t.path();
    let now = 1_800_000_000;
    let r7 = now + 86_400 * 5;
    let iso = iso_z;
    let body = |week: f64| json!({"seven_day":{"utilization":week,"resets_at":iso(r7)}});
    let replay = |w: f64| {
        json!({"session_id":"s","version":"2.1.314","rate_limits":{
            "seven_day":{"used_percentage":w,"resets_at":r7}}})
    };
    super::super::record_statusline(home, &replay(20.0), now).unwrap();
    let week = |at: i64| super::super::project_usage(home, at).0["weekly"].clone();
    let natural = week(now)["cycle"].clone();
    // A single lower answer: kept out (usage.json is not rewritten).
    let before = std::fs::read(super::super::usage_file(home)).unwrap();
    assert!(!record_answer(home, &body(10.0), now + 10).unwrap());
    assert_eq!(
        std::fs::read(super::super::usage_file(home)).unwrap(),
        before
    );
    assert_eq!(week(now + 11)["used_percent"], 20.0);
    // Too soon is no confirmation.
    record_answer(home, &body(10.0), now + 30).unwrap();
    assert_eq!(week(now + 31)["used_percent"], 20.0);
    // Confirmed: accepted, a new cycle.
    assert!(record_answer(home, &body(10.0), now + 10 + CONFIRM).unwrap());
    let w = week(now + 11 + CONFIRM);
    assert_eq!(w["used_percent"], 10.0);
    assert_eq!(w["cycle_reason"], "provider_lowered");
    assert_ne!(w["cycle"], natural);
    // The stale value replayed by an idle session stays out afterwards.
    super::super::record_statusline(home, &replay(20.0), now + 200).unwrap();
    assert_eq!(week(now + 201)["used_percent"], 10.0);
}
const CONFIRM: i64 = crate::provider::usage_cycle::CONFIRM_SECONDS;
/// RFC 3339 (`Z`) of Unix seconds (Howard Hinnant's civil date).
fn iso_z(t: i64) -> String {
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}
