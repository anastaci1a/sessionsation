use super::*;
use crate::{
    model::{State, Usage, Window},
    paths::Paths,
};
use serde_json::json;

fn state() -> State {
    let mut state = State::default();
    for name in ["personal", "work", "third"] {
        let mut a = Account::new(name.into(), String::new());
        a.switch_thresholds.observe_plan(Some("plus"));
        a.usage = Usage {
            observed_at: 100,
            short: Some(Window {
                used_percent: 10.,
                duration_minutes: 300,
                resets_at: 999,
            }),
            weekly: Some(Window {
                used_percent: 20.,
                duration_minutes: 10080,
                resets_at: 999,
            }),
            ..Default::default()
        };
        state.accounts.insert(name.into(), a);
    }
    state
}
fn policy() -> Policy {
    Policy {
        accounts: vec!["personal".into(), "work".into(), "third".into()],
        ..Default::default()
    }
}

#[test]
fn default_reserves_scale_and_custom_reserves_survive_clamping_and_storage() {
    let mut t = Thresholds::default();
    t.observe_plan(Some("plus"));
    assert_eq!(t.short_percent, 90.);
    t.observe_plan(Some("pro_5x"));
    assert_eq!(t.short_percent, 98.);
    t.observe_plan(Some("pro_20x"));
    assert_eq!(t.short_percent, 99.5);
    t.set_percentages(70., 80.).unwrap();
    t.observe_plan(Some("plus"));
    assert_eq!(t.short_percent, 0.);
    let mut t: Thresholds = serde_json::from_value(serde_json::to_value(t).unwrap()).unwrap();
    t.observe_plan(Some("pro_20x"));
    assert_eq!((t.short_percent, t.weekly_percent), (70., 80.));
}
#[test]
fn generic_pro_requires_explicit_tier_and_a_changed_plan_invalidates_override() {
    let mut t = Thresholds::default();
    t.observe_plan(Some("pro"));
    assert!(t.needs_plan_review);
    t.set_multiplier(5.).unwrap();
    assert_eq!(t.short_percent, 98.);
    t.observe_plan(Some("pro"));
    assert!(!t.needs_plan_review);
    t.observe_plan(Some("enterprise"));
    assert!(t.needs_plan_review);
    t.set_multiplier(20.).unwrap();
    assert_eq!(t.short_percent, 99.5);
    assert!(t.set_multiplier(0.).is_err());
    assert!(t.set_multiplier(f64::NAN).is_err());
    assert!(t.set_percentages(101., 50.).is_err());
    assert!(t.set_percentages(0., f64::NAN).is_err());
}
#[test]
fn rotation_stays_then_uses_selected_order_and_pauses_when_none_fit() {
    let mut s = state();
    let p = policy();
    let c = Config::default();
    assert_eq!(
        preview(&p, "personal", &s.accounts, &c, 100),
        Preview::Stay {
            account: "personal".into()
        }
    );
    s.accounts
        .get_mut("personal")
        .unwrap()
        .usage
        .weekly
        .as_mut()
        .unwrap()
        .used_percent = 90.;
    assert_eq!(
        preview(&p, "personal", &s.accounts, &c, 100),
        Preview::Switch {
            account: "work".into()
        }
    );
    s.accounts.get_mut("work").unwrap().auth_required = true;
    assert_eq!(
        preview(&p, "personal", &s.accounts, &c, 100),
        Preview::Switch {
            account: "third".into()
        }
    );
    s.accounts
        .get_mut("third")
        .unwrap()
        .usage
        .short
        .as_mut()
        .unwrap()
        .used_percent = 90.;
    assert_eq!(
        preview(&p, "personal", &s.accounts, &c, 100),
        Preview::Pause
    );
    s.accounts.get_mut("personal").unwrap().usage.observed_at = 0;
    assert!(matches!(
        preview(&p, "personal", &s.accounts, &c, 100),
        Preview::Wait { .. }
    ));
}
#[test]
fn neither_credits_nor_stale_data_override_thresholds() {
    let mut s = state();
    let p = policy();
    let c = Config::default();
    let a = s.accounts.get_mut("personal").unwrap();
    a.usage.short.as_mut().unwrap().used_percent = 90.;
    a.usage.credits = Some(crate::model::Credits {
        unlimited: true,
        ..Default::default()
    });
    s.accounts.get_mut("work").unwrap().usage.observed_at = 0;
    s.accounts
        .get_mut("third")
        .unwrap()
        .switch_thresholds
        .needs_plan_review = true;
    assert_eq!(
        preview(&p, "personal", &s.accounts, &c, 100),
        Preview::Pause
    );
    assert!(
        Policy {
            accounts: vec!["personal".into(), "personal".into()],
            ..Default::default()
        }
        .validate(&s.accounts)
        .is_err()
    );
}
#[test]
fn consent_disable_and_crash_reconciliation_never_replay_reservations() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().to_owned())).unwrap();
    let mut s = state();
    let config = Config::default();
    let thread = "11111111-1111-4111-8111-111111111111";
    request(
        &paths,
        &mut s,
        &config,
        "automatic/set",
        &json!({"thread":thread,"accounts":["personal","work"]}),
    )
    .unwrap();
    assert!(
        request(
            &paths,
            &mut s,
            &config,
            "automatic/enable",
            &json!({"thread":thread})
        )
        .is_err()
    );
    request(
        &paths,
        &mut s,
        &config,
        "automatic/enable",
        &json!({"thread":thread,"accept_experimental":true}),
    )
    .unwrap();
    assert!(s.automatic_switching[thread].enabled);
    let revision = s.automatic_switching[thread].revision;
    request(
        &paths,
        &mut s,
        &config,
        "automatic/disable",
        &json!({"thread":thread}),
    )
    .unwrap();
    assert!(!s.automatic_switching[thread].enabled);
    assert!(request(&paths,&mut s,&config,"automatic/reserve",&json!({"thread":thread,"revision":revision,"current_account":"personal","target":"work"})).is_err());
    let p = s.automatic_switching.get_mut(thread).unwrap();
    p.enabled = true;
    p.pending_job = Some("reserved".into());
    reconcile(&mut s);
    let p = &s.automatic_switching[thread];
    assert!(!p.enabled);
    assert!(p.pending_job.is_none());
    assert_eq!(p.last_job.as_deref(), Some("reserved"));
    reconcile(&mut s);
    assert!(!s.automatic_switching[thread].enabled);
}
#[test]
fn old_state_loads_with_automation_off() {
    let mut value = serde_json::to_value(state()).unwrap();
    value.as_object_mut().unwrap().remove("automatic_switching");
    for a in value["accounts"].as_object_mut().unwrap().values_mut() {
        a.as_object_mut().unwrap().remove("switch_thresholds");
    }
    let s: State = serde_json::from_value(value).unwrap();
    assert!(s.automatic_switching.is_empty());
    assert!(s.accounts["personal"].switch_thresholds.needs_plan_review);
}
