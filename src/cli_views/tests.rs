use super::*;
#[test]
fn account_remaining_preserves_freshness_and_unknown_windows() {
    let s = json!({"observed_at":123,"accounts":[{"name":"work","usage_fresh":false,"usage":{"weekly":{"used_percent":42},"short":null}}]});
    let v = account(&s, "codex", "work").unwrap();
    assert_eq!(v["remaining_percent"]["weekly"], 58.0);
    assert!(v["remaining_percent"]["short"].is_null());
    assert_eq!(v["account"]["usage_fresh"], false);
    assert!(account(&s, "codex", "missing").is_err());
}
#[test]
fn history_ranges_match_ui_and_reject_unbounded_requests() {
    let (s, e, step) = history_range(180000, 12, None, None, None).unwrap();
    assert_eq!(e - s, 12 * 3600);
    assert_eq!(step, 1800);
    let (s, e, step) = history_range(900000, 168, None, None, None).unwrap();
    assert_eq!(e - s, 168 * 3600);
    assert_eq!(step, 21600);
    assert!(history_range(0, 12, Some(-1), Some(2), None).is_err());
    assert!(history_range(0, 12, Some(0), Some(i64::MAX), None).is_err());
    assert!(history_range(0, 12, None, Some(i64::MIN), None).is_err());
    assert!(history_range(0, 12, Some(0), Some(RETENTION + 1), None).is_err());
    assert!(history_range(900000, 168, None, None, Some(1)).is_err());
    assert!(history_range(900000, 168, None, None, Some(1800)).is_err());
}
