use super::*;

fn reading(w: &Value, at: i64, replayable: bool) -> Reading<'_> {
    Reading {
        window: w,
        at,
        source: if replayable { "statusline" } else { "oauth" },
        replayable,
        first_use: false,
    }
}
fn w(used: f64, resets: i64) -> Value {
    json!({"used_percentage":used,"resets_at":resets})
}

#[test]
fn a_cycle_never_goes_down_and_a_redeem_starts_one_that_accepts_the_lower_reading() {
    let (now, r) = (1_000_000, 1_000_000 + 86_400);
    let week = 10_080 * 60;
    let mut s = Slot::default();
    assert!(s.apply(&reading(&w(20.0, r), now, true), now, week));
    let natural = s.cycle.clone().unwrap();
    assert_eq!(natural.reason, NATURAL);
    assert_eq!(natural.started_at, r - week);
    // A lower reading of the same cycle is stale.
    assert!(!s.apply(&reading(&w(10.0, r), now + 60, true), now + 60, week));
    assert_eq!(s.window["used_percentage"], 20.0);
    // A redeem starts a new cycle with the same resets_at.
    assert!(s.start(now + 100, REDEEM, week));
    let redeemed = s.cycle.clone().unwrap();
    assert_ne!(redeemed.id, natural.id);
    assert_eq!(redeemed.started_at, now + 100);
    assert_eq!(redeemed.resets_at, r);
    // While it is open: a replay of the pre-redeem response, a lower statusline reading of
    // another session and an OAuth answer sent before the redeem are kept out.
    assert!(!s.apply(&reading(&w(20.0, r), now + 110, true), now + 110, week));
    assert!(!s.apply(&reading(&w(5.0, r), now + 112, true), now + 112, week));
    assert!(!s.apply(&reading(&w(10.0, r), now + 99, false), now + 115, week));
    assert!(s.cycle.as_ref().unwrap().open);
    // The first OAuth answer after it closes the cycle, lower.
    assert!(s.apply(&reading(&w(10.0, r), now + 120, false), now + 120, week));
    assert_eq!(s.window["used_percentage"], 10.0);
    assert_eq!(s.cycle.as_ref().unwrap().id, redeemed.id);
    assert!(!s.cycle.as_ref().unwrap().open);
    // Then the rule holds again, and the replay stays out (an older recorder rewriting the
    // pre-redeem value is one such reading).
    assert!(!s.apply(&reading(&w(9.0, r), now + 130, true), now + 130, week));
    assert!(!s.apply(&reading(&w(20.0, r), now + 140, true), now + 140, week));
    assert_eq!(s.window["used_percentage"], 10.0);
    // A reading older than the recorded one is kept out.
    assert!(!s.apply(&reading(&w(15.0, r), now + 115, true), now + 145, week));
    // Real growth past the old value is accepted.
    assert!(s.apply(&reading(&w(21.0, r), now + 150, true), now + 150, week));
    // A natural reset is a new cycle.
    let next = w(1.0, r + week);
    assert!(s.apply(&reading(&next, now + 160, true), now + 160, week));
    let c = s.cycle.unwrap();
    assert_eq!((c.reason.as_str(), c.started_at), (NATURAL, r));
    assert_eq!(s.window["used_percentage"], 1.0);
}

#[test]
fn a_statusline_reading_closes_an_open_cycle_only_when_not_lower() {
    let (now, r, week) = (1_000_000, 1_000_000 + 86_400, 10_080 * 60);
    let mut s = Slot::default();
    s.apply(&reading(&w(20.0, r), now, true), now, week);
    s.start(now + 10, REDEEM, week);
    assert!(s.apply(&reading(&w(22.0, r), now + 20, true), now + 20, week));
    assert!(!s.cycle.as_ref().unwrap().open);
    assert_eq!(s.window["used_percentage"], 22.0);
}

#[test]
fn the_replay_filter_ends_once_an_oauth_reading_is_back_at_the_prior_value() {
    let (now, r, week) = (1_000_000, 1_000_000 + 86_400, 10_080 * 60);
    let mut s = Slot::default();
    s.apply(&reading(&w(20.0, r), now, true), now, week);
    s.start(now + 10, REDEEM, week);
    s.apply(&reading(&w(10.0, r), now + 20, false), now + 20, week);
    assert_eq!(s.cycle.as_ref().unwrap().prior_percent, Some(20.0));
    // Usage really grows back: the OAuth read sees 20, the filter ends, and the statusline
    // value of exactly 20 counts again.
    assert!(s.apply(&reading(&w(20.0, r), now + 30, false), now + 30, week));
    assert_eq!(s.cycle.as_ref().unwrap().prior_percent, None);
    // A statusline rise above the fresh OAuth reading waits for a second one 60 s later.
    s.apply(&reading(&w(21.0, r), now + 40, true), now + 40, week);
    assert_eq!(s.window["source"], "oauth");
    assert!(s.apply(&reading(&w(21.0, r), now + 100, true), now + 100, week));
    assert_eq!(s.window["source"], "statusline");
}

#[test]
fn a_lower_oauth_reading_is_accepted_only_when_a_second_read_confirms_it() {
    let (now, r, week) = (1_000_000, 1_000_000 + 86_400, 10_080 * 60);
    let mut s = Slot::default();
    s.apply(&reading(&w(20.0, r), now, true), now, week);
    let natural = s.cycle.clone().unwrap().id;
    // One lower OAuth answer: kept out, waiting.
    s.apply(&reading(&w(10.0, r), now + 10, false), now + 10, week);
    assert_eq!(s.window["used_percentage"], 20.0);
    assert_eq!(
        s.cycle.as_ref().unwrap().pending_lower,
        Some((10.0, now + 10))
    );
    // A second one too soon is no confirmation.
    s.apply(&reading(&w(10.0, r), now + 30, false), now + 30, week);
    assert_eq!(s.window["used_percentage"], 20.0);
    // A not-lower OAuth answer clears the wait.
    s.apply(&reading(&w(20.0, r), now + 40, false), now + 40, week);
    assert_eq!(s.cycle.as_ref().unwrap().pending_lower, None);
    s.apply(&reading(&w(10.0, r), now + 50, false), now + 50, week);
    // Confirmed at least CONFIRM_SECONDS later with the same or less: a new cycle.
    assert!(s.apply(
        &reading(&w(9.0, r), now + 50 + CONFIRM_SECONDS, false),
        now + 50 + CONFIRM_SECONDS,
        week
    ));
    assert_eq!(s.window["used_percentage"], 9.0);
    let c = s.cycle.clone().unwrap();
    assert_eq!(c.reason, PROVIDER_LOWERED);
    assert_ne!(c.id, natural);
    assert_eq!(c.prior_percent, Some(20.0));
    // A statusline replay of the stale value stays out after the heal.
    assert!(!s.apply(&reading(&w(20.0, r), now + 200, true), now + 200, week));
    // A confirmation with more usage than the waiting reading does not lower.
    let mut t = Slot::default();
    t.apply(&reading(&w(20.0, r), now, true), now, week);
    t.apply(&reading(&w(10.0, r), now + 10, false), now + 10, week);
    t.apply(&reading(&w(12.0, r), now + 100, false), now + 100, week);
    assert_eq!(t.window["used_percentage"], 20.0);
}

#[test]
fn a_cleared_window_with_no_current_one_starts_on_first_use() {
    let (now, r5) = (1_000_000, 1_000_000 + 3_600);
    let five = 300 * 60;
    let mut s = Slot::default();
    s.apply(&reading(&w(80.0, r5), now, true), now, five);
    assert!(s.start(now + 10, REDEEM, five));
    let id = s.cycle.clone().unwrap().id;
    let none = Value::Null;
    let cleared = Reading {
        window: &none,
        at: now + 20,
        source: "oauth",
        replayable: false,
        first_use: true,
    };
    // After a redeem: 0% until first use, still the redeem's cycle.
    assert!(s.apply(&cleared, now + 20, five));
    assert!(s.first_use());
    assert_eq!(s.window["used_percentage"], 0.0);
    assert!(s.window["resets_at"].is_null());
    assert_eq!(s.cycle.as_ref().unwrap().id, id);
    // The replay of the old window stays out; the next window keeps the redeem's cycle.
    assert!(!s.apply(&reading(&w(80.0, r5), now + 30, true), now + 30, five));
    let fresh = w(2.0, now + 40 + five);
    assert!(s.apply(&reading(&fresh, now + 40, true), now + 40, five));
    assert_eq!(s.cycle.as_ref().unwrap().id, id);
    // After the window's own reset on an idle account: a new natural cycle at 0%.
    let after = now + 41 + five;
    let idle = Reading {
        window: &none,
        at: after,
        source: "oauth",
        replayable: false,
        first_use: true,
    };
    assert!(s.apply(&idle, after, five));
    let c = s.cycle.clone().unwrap();
    assert_eq!(c.reason, NATURAL);
    assert_ne!(c.id, id);
    assert!(s.first_use());
    // A not-started answer while a recorded window still runs is kept out.
    let mut t = Slot::default();
    t.apply(&reading(&w(30.0, r5), now, true), now, five);
    assert!(!t.apply(&cleared, now + 20, five));
    assert_eq!(t.window["used_percentage"], 30.0);
}

#[test]
fn codex_cycles_change_on_new_resets_redeems_and_lowered_readings() {
    let (at, r, five) = (1_000, 20_000, 300 * 60);
    let first = codex(None, None, Some((23.0, r)), five, at, None).unwrap();
    assert_eq!(first.reason, NATURAL);
    // Clock drift within a cycle keeps it.
    let same = codex(
        Some(&first),
        Some((23.0, r)),
        Some((24.0, r + 3)),
        five,
        at + 1,
        None,
    );
    assert_eq!(same.as_ref().unwrap().id, first.id);
    let lowered = codex(
        same.as_ref(),
        Some((24.0, r + 3)),
        Some((5.0, r + 4)),
        five,
        at + 2,
        None,
    )
    .unwrap();
    assert_eq!(lowered.reason, PROVIDER_LOWERED);
    let redeemed = codex(
        Some(&lowered),
        Some((5.0, r)),
        Some((0.0, r)),
        five,
        at + 3,
        Some(REDEEM),
    )
    .unwrap();
    assert_eq!(
        (redeemed.reason.as_str(), redeemed.started_at),
        (REDEEM, at + 3)
    );
    let next = codex(
        Some(&redeemed),
        Some((0.0, r)),
        Some((1.0, r + five)),
        five,
        at + 4,
        None,
    )
    .unwrap();
    assert_eq!((next.reason.as_str(), next.started_at), (NATURAL, r));
    // An absent window keeps its cycle.
    assert_eq!(
        codex(Some(&next), None, None, five, at + 5, None),
        Some(next)
    );
}

const NOW: i64 = 1_000_000;
const RESETS: i64 = NOW + 86_400;
const WEEK: i64 = 10_080 * 60;

#[test]
fn a_stale_statusline_rise_does_not_undo_a_confirmed_oauth_zero() {
    let mut s = Slot::default();
    s.apply(&reading(&w(95.0, RESETS), NOW, true), NOW, WEEK);
    s.apply(&reading(&w(0.0, RESETS), NOW + 10, false), NOW + 10, WEEK);
    s.apply(&reading(&w(0.0, RESETS), NOW + 80, false), NOW + 80, WEEK);
    assert_eq!(s.cycle.as_ref().unwrap().reason, PROVIDER_LOWERED);
    s.apply(&reading(&w(95.0, RESETS), NOW + 100, true), NOW + 100, WEEK);
    assert_eq!(s.window["used_percentage"], 0.0);
}

#[test]
fn a_confirmed_statusline_rise_above_a_fresh_oauth_reading_is_real_growth() {
    let mut s = Slot::default();
    s.apply(&reading(&w(10.0, RESETS), NOW, false), NOW, WEEK);
    s.apply(&reading(&w(30.0, RESETS), NOW + 20, true), NOW + 20, WEEK);
    assert_eq!(s.window["used_percentage"], 10.0);
    s.apply(
        &reading(&w(30.0, RESETS), NOW + 20 + CONFIRM_SECONDS, true),
        NOW + 80,
        WEEK,
    );
    assert_eq!(s.window["used_percentage"], 30.0);
}

#[test]
fn a_statusline_rise_above_an_old_oauth_reading_is_accepted() {
    let mut s = Slot::default();
    s.apply(&reading(&w(10.0, RESETS), NOW, false), NOW, WEEK);
    let late = NOW + OAUTH_FRESH_SECONDS + 1;
    s.apply(&reading(&w(30.0, RESETS), late, true), late, WEEK);
    assert_eq!(s.window["used_percentage"], 30.0);
}

#[test]
fn idle_statusline_replays_near_the_prior_never_confirm_a_rise_after_a_redeem() {
    let mut s = Slot::default();
    s.apply(&reading(&w(95.0, RESETS), NOW, true), NOW, WEEK);
    s.apply(&reading(&w(0.0, RESETS), NOW + 10, false), NOW + 10, WEEK);
    s.apply(&reading(&w(0.0, RESETS), NOW + 80, false), NOW + 80, WEEK);
    for at in [NOW + 90, NOW + 151] {
        s.apply(&reading(&w(94.0, RESETS), at, true), at, WEEK);
        assert_eq!(s.window["used_percentage"], 0.0);
    }
    let at = NOW + 160;
    s.apply(&reading(&w(50.0, RESETS), at, false), at, WEEK);
    assert_eq!(s.window["used_percentage"], 50.0);
}

#[test]
fn a_statusline_reading_below_the_prior_window_behaves_as_before() {
    let mut s = Slot::default();
    s.apply(&reading(&w(95.0, RESETS), NOW, true), NOW, WEEK);
    s.apply(&reading(&w(0.0, RESETS), NOW + 10, false), NOW + 10, WEEK);
    s.apply(&reading(&w(0.0, RESETS), NOW + 80, false), NOW + 80, WEEK);
    s.apply(&reading(&w(30.0, RESETS), NOW + 90, true), NOW + 90, WEEK);
    assert_eq!(s.window["used_percentage"], 0.0);
    s.apply(&reading(&w(30.0, RESETS), NOW + 150, true), NOW + 150, WEEK);
    assert_eq!(s.window["used_percentage"], 30.0);
}
