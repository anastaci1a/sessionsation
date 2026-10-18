use super::*;
use crate::{model::Window, paths::Paths};

fn fixture() -> (tempfile::TempDir, Paths, Store, BTreeMap<String, Account>) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(temp.path().join("data"))).unwrap();
    let store = Store::open(&paths).unwrap();
    let mut account = Account::new("personal".into(), "fixture".into());
    account.usage = Usage {
        short: Some(Window {
            used_percent: 25.,
            duration_minutes: 300,
            resets_at: 18000,
        }),
        weekly: Some(Window {
            used_percent: 40.,
            duration_minutes: 10080,
            resets_at: 900000,
        }),
        observed_at: 1800,
        reported_windows: Some(vec![300, 10080]),
        ..Default::default()
    };
    (
        temp,
        paths,
        store,
        BTreeMap::from([("personal".into(), account)]),
    )
}
fn activity(active: u32) -> Option<BTreeMap<String, Counts>> {
    Some(BTreeMap::from([(
        "personal".into(),
        Counts { active, unknown: 0 },
    )]))
}
#[test]
fn half_hour_samples_survive_restart_and_do_not_reuse_old_or_reset_readings() {
    let (_temp, paths, mut store, mut accounts) = fixture();
    let mut recorder = Recorder::default();
    recorder
        .observe(&mut store, &accounts, 1800, 210, activity(0))
        .unwrap();
    accounts
        .get_mut("personal")
        .unwrap()
        .usage
        .short
        .as_mut()
        .unwrap()
        .used_percent = 80.;
    recorder
        .observe(&mut store, &accounts, 1830, 210, activity(0))
        .unwrap();
    recorder
        .observe(&mut store, &accounts, 3600, 210, activity(0))
        .unwrap();
    let history = query(
        &store.db,
        "personal",
        &accounts["personal"].usage,
        1800,
        5400,
        1800,
    )
    .unwrap();
    assert_eq!(history.bins[0].short, Some(75.));
    assert_eq!(
        history.bins[1].short, None,
        "stale data must not fill a new half hour"
    );
    let u = &mut accounts.get_mut("personal").unwrap().usage;
    u.observed_at = 3630;
    u.short.as_mut().unwrap().resets_at = 3600;
    recorder
        .observe(&mut store, &accounts, 3630, 210, activity(0))
        .unwrap();
    drop(store);
    let store = Store::open(&paths).unwrap();
    let history = query(
        &store.db,
        "personal",
        &accounts["personal"].usage,
        1800,
        5400,
        1800,
    )
    .unwrap();
    assert_eq!(history.bins[0].short, Some(75.));
    assert_eq!(
        history.bins[1].short, None,
        "elapsed reset is not fresh capacity"
    );
    assert_eq!(history.bins[1].weekly, Some(60.));
    assert!(query(&store.db, "personal", &Usage::default(), 0, 3600, i64::MAX).is_err());
}
#[test]
fn active_durations_split_at_boundaries_and_exclude_downtime_restarts_and_clock_reversal() {
    let (_temp, _paths, mut store, accounts) = fixture();
    let mut recorder = Recorder::default();
    for (at, n) in [(1770, 2), (1800, 1), (1830, 0), (2000, 3), (2030, 3)] {
        recorder
            .observe(&mut store, &accounts, at, 210, activity(n))
            .unwrap();
    }
    let bins = query(&store.db, "personal", &Usage::default(), 0, 3600, 1800)
        .unwrap()
        .bins;
    assert_eq!(
        (
            bins[0].active_seconds,
            bins[0].session_seconds,
            bins[0].peak
        ),
        (30, 60, 2)
    );
    assert_eq!(
        (
            bins[1].active_seconds,
            bins[1].session_seconds,
            bins[1].observed_seconds
        ),
        (60, 120, 60)
    );
    let mut recorder = Recorder::default();
    for at in [2060, 2090, 2070, 2100] {
        recorder
            .observe(&mut store, &accounts, at, 210, activity(1))
            .unwrap();
    }
    recorder
        .observe(&mut store, &accounts, 2130, 210, None)
        .unwrap();
    recorder
        .observe(&mut store, &accounts, 2160, 210, activity(1))
        .unwrap();
    let bin = query(&store.db, "personal", &Usage::default(), 1800, 3600, 1800)
        .unwrap()
        .bins
        .remove(0);
    assert_eq!(bin.observed_seconds, 100);
    assert_eq!(bin.active_seconds, 100);
    assert_eq!(bin.session_seconds, 160);
    assert_eq!(bin.periods.len(), 3);
}
#[test]
fn weekly_only_and_missing_tails_are_not_misrepresented() {
    let (_temp, _paths, mut store, mut accounts) = fixture();
    let u = &mut accounts.get_mut("personal").unwrap().usage;
    u.short = None;
    u.reported_windows = Some(vec![10080]);
    Recorder::default()
        .observe(&mut store, &accounts, 1800, 210, activity(0))
        .unwrap();
    let history = query(
        &store.db,
        "personal",
        &accounts["personal"].usage,
        1800,
        3600,
        1800,
    )
    .unwrap();
    assert!(history.short_absent);
    assert_eq!(history.bins[0].weekly, Some(60.));
    let history = query(
        &store.db,
        "personal",
        &accounts["personal"].usage,
        1800,
        7200,
        5400,
    )
    .unwrap();
    assert_eq!(
        history.bins[0].weekly, None,
        "an outage at the tail must remain a gap"
    );
    assert_eq!(history.bins[0].samples, 1);
}
#[test]
fn retention_is_bounded_and_other_accounts_stay_separate() {
    let (_temp, _paths, mut store, mut accounts) = fixture();
    let mut other = accounts["personal"].clone();
    other.name = "work".into();
    accounts.insert(other.name.clone(), other);
    let mut recorder = Recorder::default();
    for at in [1800, 1830] {
        recorder
            .observe(&mut store, &accounts, at, 210, activity(2))
            .unwrap();
    }
    let work = query(&store.db, "work", &Usage::default(), 1800, 3600, 1800).unwrap();
    assert_eq!(work.bins[0].peak, 0);
    assert_eq!(work.bins[0].observed_seconds, 30);
    recorder
        .observe(&mut store, &accounts, RETENTION + 86400, 210, activity(0))
        .unwrap();
    let old = query(&store.db, "personal", &Usage::default(), 1800, 3600, 1800).unwrap();
    assert_eq!(old.bins[0].samples, 0);
    assert_eq!(old.bins[0].observed_seconds, 0);
}
#[test]
fn duplicate_panes_count_once_and_idle_saved_and_unknown_are_distinct() {
    fn session(id: &str, status: Activity) -> Instance {
        let mut s:Instance=serde_json::from_value(serde_json::json!({
            "account":"personal","pane":"%1","tmux_session":"fixture","pid":1,
            "cwd":"/fixture","thread_id":id,"rollout":"/fixture/rollout","home":"/fixture","binary":"fake","resume_options":[]
        })).unwrap();
        s.activity = status;
        s
    }
    let busy = session("one", Activity::Busy);
    let mut saved = session("saved", Activity::Busy);
    saved.inactive = true;
    let result = counts(&[
        busy.clone(),
        busy,
        session("one", Activity::Unknown),
        session("two", Activity::Unknown),
        session("idle", Activity::Idle),
        saved,
    ]);
    assert_eq!(
        result["personal"],
        Counts {
            active: 1,
            unknown: 1
        }
    );
}
