use super::super::*;
#[test]
fn cached_codex_usage_is_refreshed_from_the_supervisor_and_never_ahead_of_now() {
    let row = |at: i64, pct: f64| {
        json!({"name":"p","provider":"codex","usage_fresh":true,"usage_state":"fresh","credit_fresh":false,
        "usage":{"observed_at":at,"weekly":{"used_percent":pct,"resets_at":9999}}})
    };
    let mut cached = json!({"accounts":[row(1000, 64.)]});
    let status = json!({"accounts":[row(2000, 75.)]});
    overlay_codex_usage(&mut cached, Some(&status), 2001, 300);
    assert_eq!(
        cached["accounts"][0]["usage"]["weekly"]["used_percent"],
        75.
    );
    // Supervisor unreachable: the frozen value ages out instead of staying fresh.
    let mut cached = json!({"accounts":[row(1000, 64.)]});
    overlay_codex_usage(&mut cached, None, 5000, 300);
    assert_eq!(cached["accounts"][0]["usage_state"], "stale");
    assert_eq!(cached["accounts"][0]["usage_fresh"], false);
    // A future timestamp is clamped and not fresh.
    let mut cached = json!({"accounts":[row(1191, 64.)]});
    overlay_codex_usage(&mut cached, None, 1000, 300);
    assert_eq!(cached["accounts"][0]["usage"]["observed_at"], 1000);
    assert_eq!(cached["accounts"][0]["usage_fresh"], false);
}
