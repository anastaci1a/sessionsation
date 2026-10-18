use serde_json::json;
use sessionsation::{
    config::Config,
    daemon::parse_usage,
    model::*,
    scheduler::*,
    simulation::{self, Effect, Input},
};

fn usage(short: f64, weekly: f64, at: i64) -> Usage {
    Usage {
        short: Some(Window {
            used_percent: short,
            duration_minutes: 300,
            resets_at: 20000,
        }),
        weekly: Some(Window {
            used_percent: weekly,
            duration_minutes: 10080,
            resets_at: 1000000,
        }),
        observed_at: at,
        exhausted: false,
        ..Default::default()
    }
}

#[test]
fn native_single_window_plans_follow_reported_limits() {
    for (plan, duration) in [("pro", 300), ("prolite", 10080)] {
        let response = json!({"rateLimits":{"planType":plan,"primary":{
            "usedPercent":41,"windowDurationMins":duration,"resetsAt":20000
        },"secondary":null}});
        let mut a = Account::new("test".into(), "fixture".into());
        a.observe(parse_usage(&response, 1000).unwrap());
        let config = Config::default();
        assert_eq!(a.usage.plan_type.as_deref(), Some(plan));
        assert!(a.usage.fresh(1001, config.stale_seconds));
        assert!(eligible(&a, &config, 1001));
        assert!(pressure(&a, &config).is_finite());
        let absent = if duration == 300 { 10080 } else { 300 };
        assert!(a.usage.window_absent(absent));
        assert!(!a.usage.window_absent(duration));
        assert!(!eligible(&a, &config, 20001));
        if let Some(w) = a.usage.short.as_mut().or(a.usage.weekly.as_mut()) {
            w.used_percent = 100.;
        }
        assert!(!eligible(&a, &config, 1001));
    }
}

#[test]
fn missing_or_unrecognized_windows_do_not_imply_unlimited_capacity() {
    for limits in [
        json!({"planType":"pro","primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":20000}}),
        json!({"planType":"pro","primary":null,"secondary":null}),
        json!({"primary":{"usedPercent":10,"windowDurationMins":60,"resetsAt":20000},"secondary":null}),
    ] {
        let usage = parse_usage(&json!({"rateLimits":limits}), 1000).unwrap();
        assert!(!usage.fresh(1001, 300));
        assert!(!usage.window_absent(10080));
    }
    let mut a = account("test", 20., 100.);
    a.usage.plan_type = Some("pro".into());
    assert!(!eligible(&a, &Config::default(), 1001));
    assert!(!a.usage.window_absent(10080));
}
fn account(name: &str, short: f64, weekly: f64) -> Account {
    let mut a = Account::new(name.into(), name.into());
    a.observe(usage(short, weekly, 1000));
    a
}
fn session(id: &str, account: &str) -> Session {
    Session {
        id: id.into(),
        root_thread: Some(format!("root-{id}")),
        cwd: "/tmp/project".into(),
        account: Some(account.into()),
        phase: Phase::Running,
        assigned_at: 0,
        waiting_since: 0,
        manual_pause: false,
        resume_work: false,
        reason: None,
        handoff: None,
        tmux_pane: None,
        codex_args: vec![],
    }
}
fn state() -> State {
    let mut s = State::default();
    s.accounts.insert("a".into(), account("a", 20., 10.));
    s.accounts.insert("b".into(), account("b", 0., 0.));
    s.sessions.insert("s".into(), session("s", "a"));
    s
}
fn quiescent() -> Quiescence {
    Quiescence {
        complete_tree_observed: true,
        all_turns_ended: true,
        no_running_tools: true,
        no_pending_approvals: true,
        history_flushed: true,
        no_ambiguous_side_effects: true,
    }
}

#[test]
fn one_session_balances_across_two_accounts() {
    let mut s = state();
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(25., 10., 1000));
    assert!(
        matches!(decide(&s,&Config::default(),1000),Some(Decision::Pause{destination:Some(a),..}) if a=="b")
    );
}
#[test]
fn below_increment_stays_put() {
    assert!(decide(&state(), &Config::default(), 1000).is_none());
}
#[test]
fn dwell_blocks_balancing_but_not_ceiling_protection() {
    let mut s = state();
    s.sessions.get_mut("s").unwrap().assigned_at = 999;
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(25., 10., 1000));
    assert!(decide(&s, &Config::default(), 1000).is_none());
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(85., 10., 1000));
    assert!(decide(&s, &Config::default(), 1000).is_some());
}
#[test]
fn weekly_window_independently_drains() {
    let mut s = state();
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(1., 85., 1000));
    assert!(matches!(
        decide(&s, &Config::default(), 1000),
        Some(Decision::Pause { .. })
    ));
}
#[test]
fn normal_balancing_has_hysteresis() {
    let mut s = state();
    s.accounts
        .get_mut("b")
        .unwrap()
        .observe(usage(24., 10., 1000));
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(25., 10., 1000));
    assert!(decide(&s, &Config::default(), 1000).is_none());
}
#[test]
fn stale_usage_requests_pause_without_routing_to_stale_destination() {
    let s = state();
    assert!(matches!(
        decide(&s, &Config::default(), 1300),
        Some(Decision::Pause {
            destination: None,
            ..
        })
    ));
}
#[test]
fn reset_timestamp_is_not_evidence_of_capacity() {
    let mut a = account("a", 1., 1.);
    a.usage.short.as_mut().unwrap().resets_at = 1000;
    assert!(!eligible(&a, &Config::default(), 1000));
}
#[test]
fn unknown_window_fails_closed() {
    let mut a = account("a", 1., 1.);
    a.usage.weekly = None;
    assert!(!eligible(&a, &Config::default(), 1000));
}
#[test]
fn auth_and_explicit_exhaustion_exclude_accounts() {
    let mut a = account("a", 1., 1.);
    a.auth_required = true;
    assert!(!eligible(&a, &Config::default(), 1000));
    a.auth_required = false;
    a.usage.exhausted = true;
    assert!(!eligible(&a, &Config::default(), 1000));
}
#[test]
fn purchased_credits_keep_plan_exhausted_account_eligible() {
    let response = json!({"ordinaryUsageAllowed":false,"rateLimitsByLimitId":{"codex":{
        "limitId":"codex","rateLimitReachedType":"rate_limit_reached",
        "credits":{"hasCredits":true,"unlimited":false,"balance":"584.6591530000"},
        "spendControlReached":false,
        "primary":{"usedPercent":100,"windowDurationMins":300,"resetsAt":20000},
        "secondary":{"usedPercent":16,"windowDurationMins":10080,"resetsAt":1000000}
    }}});
    let mut a = account("a", 20., 16.);
    a.observe(parse_usage(&response, 1001).unwrap());
    assert!(eligible(&a, &Config::default(), 1001));
    assert_eq!(
        a.usage.credits.as_ref().unwrap().balance.as_deref(),
        Some("584.6591530000")
    );
    a.usage.spend_control_reached = Some(true);
    assert!(!eligible(&a, &Config::default(), 1001));
    a.usage.spend_control_reached = Some(false);
    a.usage.reached_type = Some("workspace_member_credits_depleted".into());
    assert!(!eligible(&a, &Config::default(), 1001));
    a.usage.reached_type = Some("rate_limit_reached".into());
    a.usage.credits.as_mut().unwrap().has_credits = false;
    assert!(!eligible(&a, &Config::default(), 1001));
}
#[test]
fn resets_do_not_count_as_consumption() {
    let mut a = account("a", 80., 60.);
    let mut next = usage(0., 0., 1001);
    next.short.as_mut().unwrap().resets_at += 18000;
    next.weekly.as_mut().unwrap().resets_at += 604800;
    a.observe(next);
    assert_eq!(a.consumed_short, 0.);
    assert_eq!(a.consumed_weekly, 0.);
}
#[test]
fn out_of_order_observation_does_not_regress_usage() {
    let mut a = account("a", 20., 20.);
    a.observe(usage(80., 80., 999));
    assert_eq!(a.usage.short.unwrap().used_percent, 20.);
}
#[test]
fn equal_pressure_prefers_lower_load() {
    let mut s = state();
    s.accounts
        .get_mut("b")
        .unwrap()
        .observe(usage(20., 10., 1000));
    assert_eq!(
        choose(&s, &Config::default(), 1000, None).unwrap().name,
        "b"
    );
}
#[test]
fn waiting_trees_resume_oldest_first() {
    let mut s = state();
    let mut a = session("old", "a");
    a.phase = Phase::WaitingCapacity;
    a.account = None;
    a.waiting_since = 1;
    let mut b = a.clone();
    b.id = "new".into();
    b.waiting_since = 2;
    s.sessions.clear();
    s.sessions.insert(a.id.clone(), a);
    s.sessions.insert(b.id.clone(), b);
    assert!(
        matches!(decide(&s,&Config::default(),1000),Some(Decision::Start{session,..}) if session=="old")
    );
}
#[test]
fn no_capacity_waits_then_resumes_on_fresh_reading() {
    let mut s = state();
    let c = Config::default();
    for a in s.accounts.values_mut() {
        a.observe(usage(90., 90., 1000));
    }
    let e = simulation::step(&mut s, &c, 1000, Input::Tick).unwrap();
    assert!(matches!(&e[0], Effect::SteerPause { .. }));
    let e = simulation::step(
        &mut s,
        &c,
        1000,
        Input::Quiescence {
            session: "s".into(),
            evidence: quiescent(),
        },
    )
    .unwrap();
    assert_eq!(
        e,
        vec![Effect::StopWorker {
            session: "s".into()
        }]
    );
    let e = simulation::step(
        &mut s,
        &c,
        1000,
        Input::WorkerExited {
            session: "s".into(),
        },
    )
    .unwrap();
    assert!(e.is_empty());
    assert_eq!(s.sessions["s"].phase, Phase::WaitingCapacity);
    let e = simulation::step(
        &mut s,
        &c,
        1001,
        Input::Usage {
            account: "b".into(),
            usage: usage(0., 0., 1001),
        },
    )
    .unwrap();
    assert_eq!(
        e,
        vec![Effect::StartWorker {
            session: "s".into(),
            account: "b".into()
        }]
    );
    let e = simulation::step(
        &mut s,
        &c,
        1001,
        Input::Restored {
            session: "s".into(),
        },
    )
    .unwrap();
    assert!(matches!(&e[0], Effect::Continue { .. }));
    simulation::step(
        &mut s,
        &c,
        1001,
        Input::ContinuationAccepted {
            session: "s".into(),
        },
    )
    .unwrap();
    assert_eq!(s.sessions["s"].phase, Phase::Running);
}
#[test]
fn no_termination_until_every_quiescence_condition_is_proven() {
    for key in [
        "complete_tree_observed",
        "all_turns_ended",
        "no_running_tools",
        "no_pending_approvals",
        "history_flushed",
        "no_ambiguous_side_effects",
    ] {
        let mut s = session("s", "a");
        begin_handoff(&mut s, Some("b".into()), "test".into(), 1000);
        let mut v = serde_json::to_value(quiescent()).unwrap();
        v[key] = json!(false);
        let q = serde_json::from_value(v).unwrap();
        assert!(!mark_quiescent(&mut s, &q));
        assert_eq!(s.phase, Phase::BlockedToolState);
        assert!(mark_quiescent(&mut s, &quiescent()));
        assert_eq!(s.phase, Phase::Quiescent);
    }
}
#[test]
fn idle_sessions_are_not_auto_continued() {
    let mut s = state();
    s.sessions.get_mut("s").unwrap().phase = Phase::Idle;
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(85., 10., 1000));
    let c = Config::default();
    simulation::step(&mut s, &c, 1000, Input::Tick).unwrap();
    simulation::step(
        &mut s,
        &c,
        1000,
        Input::Quiescence {
            session: "s".into(),
            evidence: quiescent(),
        },
    )
    .unwrap();
    simulation::step(
        &mut s,
        &c,
        1000,
        Input::WorkerExited {
            session: "s".into(),
        },
    )
    .unwrap();
    assert!(
        simulation::step(
            &mut s,
            &c,
            1000,
            Input::Restored {
                session: "s".into()
            }
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(s.sessions["s"].phase, Phase::Idle);
}
#[test]
fn destination_is_revalidated_after_draining() {
    let mut s = state();
    let c = Config::default();
    s.accounts
        .get_mut("a")
        .unwrap()
        .observe(usage(85., 10., 1000));
    simulation::step(&mut s, &c, 1000, Input::Tick).unwrap();
    simulation::step(
        &mut s,
        &c,
        1000,
        Input::Quiescence {
            session: "s".into(),
            evidence: quiescent(),
        },
    )
    .unwrap();
    s.accounts
        .get_mut("b")
        .unwrap()
        .observe(usage(99., 99., 1001));
    assert!(
        simulation::step(
            &mut s,
            &c,
            1001,
            Input::WorkerExited {
                session: "s".into()
            }
        )
        .unwrap()
        .is_empty()
    );
    assert_eq!(s.sessions["s"].phase, Phase::WaitingCapacity);
}
#[test]
fn crashes_never_blindly_replay_a_continuation() {
    let mut s = state();
    begin_handoff(
        s.sessions.get_mut("s").unwrap(),
        Some("b".into()),
        "test".into(),
        1000,
    );
    let x = s.sessions.get_mut("s").unwrap();
    x.phase = Phase::Restoring;
    x.handoff.as_mut().unwrap().continuation = Continuation::Submitting;
    let effects = simulation::step(&mut s, &Config::default(), 1000, Input::Crash).unwrap();
    assert_eq!(
        effects,
        vec![Effect::Reconcile {
            session: "s".into()
        }]
    );
    assert!(
        simulation::step(&mut s, &Config::default(), 1000, Input::Tick)
            .unwrap()
            .is_empty()
    );
}
#[test]
fn admissions_close_during_handoff() {
    let mut s = state();
    begin_handoff(s.sessions.get_mut("s").unwrap(), None, "test".into(), 1000);
    assert!(
        simulation::step(
            &mut s,
            &Config::default(),
            1000,
            Input::WorkStarted {
                session: "s".into()
            }
        )
        .is_err()
    );
}
#[test]
fn manual_pause_never_auto_resumes() {
    let mut s = state();
    let x = s.sessions.get_mut("s").unwrap();
    x.phase = Phase::WaitingCapacity;
    x.manual_pause = true;
    assert!(decide(&s, &Config::default(), 1000).is_none());
}
#[test]
fn window_order_is_not_assumed() {
    let u=parse_usage(&json!({"rateLimits":{"limitId":"codex","primary":{"usedPercent":10,"windowDurationMins":10080,"resetsAt":100000},"secondary":{"usedPercent":20,"windowDurationMins":300,"resetsAt":20000}}}),1000).unwrap();
    assert_eq!(u.short.unwrap().used_percent, 20.);
    assert_eq!(u.weekly.unwrap().used_percent, 10.);
}
#[test]
fn malformed_usage_is_not_zero_usage() {
    assert!(parse_usage(&json!({}), 1000).is_err());
    assert!(parse_usage(&json!({"rateLimits":{"primary":{"usedPercent":101,"windowDurationMins":300,"resetsAt":20000}}}),1000).is_err());
}
#[test]
fn config_validation_rejects_dangerous_and_unknown_values() {
    let c = Config {
        drain_margin: 91.,
        ..Default::default()
    };
    assert!(c.validate().is_err());
    assert!(serde_json::from_value::<Config>(json!({"force_native_switching":true})).is_err());
}
#[test]
fn native_gate_cannot_be_enabled_by_a_report() {
    assert!(sessionsation::compatibility::require_native_handoff().is_err());
}

#[test]
fn destination_reservation_prevents_parallel_admissions() {
    let mut s = state();
    s.accounts.remove("a");
    s.sessions.clear();
    for id in ["first", "second"] {
        let mut tree = session(id, "b");
        tree.phase = Phase::WaitingCapacity;
        tree.account = None;
        s.sessions.insert(id.into(), tree);
    }
    let c = Config::default();
    let effects = simulation::step(&mut s, &c, 1000, Input::Tick).unwrap();
    assert_eq!(
        effects,
        vec![Effect::StartWorker {
            session: "first".into(),
            account: "b".into()
        }]
    );
    assert!(
        simulation::step(&mut s, &c, 1000, Input::Tick)
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.sessions["second"].phase, Phase::WaitingCapacity);
}

#[test]
fn completed_assignment_requires_new_usage_before_next_admission() {
    let mut s = state();
    s.accounts.remove("a");
    s.sessions.clear();
    for id in ["first", "second"] {
        let mut tree = session(id, "b");
        tree.phase = Phase::WaitingCapacity;
        tree.account = None;
        s.sessions.insert(id.into(), tree);
    }
    let c = Config::default();
    simulation::step(&mut s, &c, 1000, Input::Tick).unwrap();
    let effects = simulation::step(
        &mut s,
        &c,
        1000,
        Input::Restored {
            session: "first".into(),
        },
    )
    .unwrap();
    assert!(effects.is_empty());
    assert_eq!(s.sessions["first"].phase, Phase::Idle);
    // Clock advancement alone is insufficient, but existing work stays admitted.
    assert!(
        simulation::step(&mut s, &c, 1001, Input::Tick)
            .unwrap()
            .is_empty()
    );
    assert!(eligible(&s.accounts["b"], &c, 1001));
    let effects = simulation::step(
        &mut s,
        &c,
        1001,
        Input::Usage {
            account: "b".into(),
            usage: usage(2., 1., 1001),
        },
    )
    .unwrap();
    assert_eq!(
        effects,
        vec![Effect::StartWorker {
            session: "second".into(),
            account: "b".into()
        }]
    );
}
