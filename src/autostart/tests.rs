use super::*;
use crate::model::Usage;
fn fixture() -> (Waiting, Account, Config) {
    let mut a = Account::new("test".into(), String::new());
    a.switch_thresholds.observe_plan(Some("plus"));
    a.usage = Usage {
        observed_at: 200,
        short: Some(Window {
            used_percent: 2.,
            duration_minutes: 300,
            resets_at: 1000,
        }),
        weekly: Some(Window {
            used_percent: 10.,
            duration_minutes: 10080,
            resets_at: 2000,
        }),
        ..Default::default()
    };
    let w = Waiting {
        event: "quota:test:turn1".into(),
        account: "test".into(),
        turn: "turn1".into(),
        pane: "%1".into(),
        pid: 1,
        start_time: 1,
        observed_at: 100,
        windows: vec![Window {
            used_percent: 100.,
            duration_minutes: 300,
            resets_at: 150,
        }],
        threshold_pause: false,
        operations: vec![],
    };
    (w, a, Config::default())
}
#[test]
fn timer_alone_or_one_of_two_resets_is_not_capacity() {
    let (mut w, mut a, c) = fixture();
    assert!(reset_ready(&w, &a, &c, 200));
    a.usage.observed_at = 100;
    assert!(!reset_ready(&w, &a, &c, 200));
    a.usage.observed_at = 200;
    a.usage.short.as_mut().unwrap().used_percent = 100.;
    assert!(!reset_ready(&w, &a, &c, 200));
    a.usage.short.as_mut().unwrap().used_percent = 2.;
    w.windows.push(Window {
        used_percent: 100.,
        duration_minutes: 10080,
        resets_at: 2000,
    });
    assert!(!reset_ready(&w, &a, &c, 200));
    a.usage.observed_at = 2100;
    a.usage.short.as_mut().unwrap().resets_at = 5000;
    a.usage.weekly.as_mut().unwrap().resets_at = 6000;
    assert!(reset_ready(&w, &a, &c, 2100));
}
#[test]
fn returned_capacity_must_respect_thresholds_and_account_state() {
    let (mut w, mut a, c) = fixture();
    w.threshold_pause = true;
    a.usage.weekly.as_mut().unwrap().used_percent = 90.;
    assert!(!reset_ready(&w, &a, &c, 200));
    a.usage.weekly.as_mut().unwrap().used_percent = 10.;
    a.auth_required = true;
    assert!(!reset_ready(&w, &a, &c, 200));
    a.auth_required = false;
    a.usage.exhausted = true;
    assert!(!reset_ready(&w, &a, &c, 200));
    a.usage.exhausted = false;
    a.switch_thresholds.needs_plan_review = true;
    assert!(!reset_ready(&w, &a, &c, 200));
}
#[test]
fn durable_reservation_disable_and_crash_never_replay() {
    let (w, a, c) = fixture();
    let mut state = State::default();
    state.accounts.insert("test".into(), a);
    let thread = "11111111-1111-4111-8111-111111111111";
    assert!(
        request(
            &mut state,
            &c,
            "auto-start/enable",
            &json!({"thread":thread})
        )
        .is_err()
    );
    request(
        &mut state,
        &c,
        "auto-start/enable",
        &json!({"thread":thread,"accept_experimental":true}),
    )
    .unwrap();
    request(
        &mut state,
        &c,
        "auto-start/observe",
        &json!({"thread":thread,"revision":1,"waiting":w}),
    )
    .unwrap();
    request(
        &mut state,
        &c,
        "auto-start/disable",
        &json!({"thread":thread}),
    )
    .unwrap();
    assert!(state.automatic_start[thread].waiting.is_none());
    assert!(
        request(
            &mut state,
            &c,
            "auto-start/reserve",
            &json!({"thread":thread,"revision":1,"event":w.event})
        )
        .is_err()
    );
    let p = state.automatic_start.get_mut(thread).unwrap();
    p.enabled = true;
    p.waiting = Some(w);
    p.sending = Some("uncertain".into());
    let mut state: State = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    reconcile(&mut state);
    let p = &state.automatic_start[thread];
    assert!(!p.enabled);
    assert!(p.waiting.is_none());
    assert!(p.sending.is_none());
    assert!(p.last_event.is_some());
}
#[test]
fn waiting_survives_restart_but_old_state_defaults_off() {
    let (w, _, _) = fixture();
    let mut state = State::default();
    state.automatic_start.insert(
        "thread".into(),
        Policy {
            enabled: true,
            waiting: Some(w),
            ..Default::default()
        },
    );
    reconcile(&mut state);
    assert!(state.automatic_start["thread"].waiting.is_some());
    let mut old = serde_json::to_value(state).unwrap();
    old.as_object_mut().unwrap().remove("automatic_start");
    assert!(
        serde_json::from_value::<State>(old)
            .unwrap()
            .automatic_start
            .is_empty()
    );
}
