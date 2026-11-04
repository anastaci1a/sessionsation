//! Opt-in per-session turn-end notification (contract 1.38.0).
//!
//! When enabled for a Codex session, the supervisor notices each turn that ends
//! (completed, interrupted or failed) and records a durable event, optionally also
//! queueing a short message to a designated session. Callers can wait on the event
//! journal with `api turn-events`. Nothing here ever steers, interrupts or
//! otherwise touches the notified session.
//!
//! Settings and the bounded event journal live in `registry/turn-notify.json`
//! (atomic replace under a lock), not in the catalog, so a session generation never
//! changes. A turn is identified by the rollout timestamp of its end, so one shorter
//! than the poll interval is still reported once; a turn is never reported twice.
use crate::{
    catalog, daemon, external,
    live_activity::{self, TurnFacts},
    paths::{Paths, atomic_write, lock_file, private_dir},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

/// Events kept in the journal; older ones are dropped (`gap:true` tells a waiter).
pub const MAX_EVENTS: usize = 500;
/// Longest `api turn-events --wait`.
pub const MAX_WAIT_SECONDS: u64 = 300;
/// Default supervisor poll interval (`SESSIONSATION_TURN_NOTIFY_SECONDS`, 0 disables).
pub const POLL_SECONDS: u64 = 5;
/// Messages delivered per poll; the rest wait for the next one.
const MAX_DELIVERIES_PER_TICK: usize = 10;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setting {
    pub enabled: bool,
    /// Session (id) that also receives a queued message for every event.
    #[serde(default)]
    pub notify_session: Option<String>,
    #[serde(default)]
    pub enabled_at: i64,
    /// The first observation after enabling only records the baseline.
    #[serde(default)]
    pub baselined: bool,
    #[serde(default)]
    pub last_state: Option<String>,
    #[serde(default)]
    pub last_ended_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub at: i64,
    pub session_id: String,
    pub name: String,
    /// `turn_ended` (completed or interrupted) or `error` (the turn failed).
    pub kind: String,
    pub error: Option<String>,
    /// null without a designated session; else the queued message's outcome.
    pub delivery: Option<Value>,
}
#[derive(Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    sessions: BTreeMap<String, Setting>,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    events: Vec<Event>,
}

fn store_file(paths: &Paths) -> PathBuf {
    paths.root.join(crate::layout::TURN_NOTIFY)
}
fn load(paths: &Paths) -> Result<Store> {
    match fs::read(store_file(paths)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("UNREADABLE_TURN_NOTIFY_SETTINGS"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err(e.into()),
    }
}
fn with_store<R>(paths: &Paths, f: impl FnOnce(&mut Store) -> Result<R>) -> Result<R> {
    private_dir(&paths.root.join(crate::layout::REGISTRY))?;
    let lock = lock_file(&paths.root.join(crate::layout::TURN_NOTIFY_LOCK))?;
    lock.lock_exclusive()?;
    let mut store = load(paths)?;
    let before = serde_json::to_vec(&store)?;
    let out = f(&mut store)?;
    store.schema_version = 1;
    let after = serde_json::to_vec(&store)?;
    if before != after {
        atomic_write(&store_file(paths), &after)?;
    }
    Ok(out)
}

/// Public projection (`api state` rows and `api turn-notify` receipts).
pub fn public(setting: Option<&Setting>) -> Value {
    let default = Setting::default();
    let s = setting.unwrap_or(&default);
    json!({"enabled":s.enabled,"notify_session":s.notify_session,"enabled_at":if s.enabled {json!(s.enabled_at)} else {Value::Null}})
}
/// Settings of every session, for `api state` (an unreadable file reads as none).
pub fn all(paths: &Paths) -> BTreeMap<String, Setting> {
    load(paths).map(|s| s.sessions).unwrap_or_default()
}
pub fn capability() -> Value {
    json!({"since_contract":"1.38.0","version":1,"command":"turn-notify",
        "args":["--session","--expected-generation","--enable","--disable","--notify-session"],
        "events_command":"turn-events","events_args":["--after","--wait"],
        "providers":["codex"],"default":"disabled","state_field":"turn_notify",
        "setting_storage":"registry/turn-notify.json","changes_session_generation":false,
        "event_kinds":["turn_ended","error"],"poll_seconds":POLL_SECONDS,"max_events":MAX_EVENTS,
        "max_wait_seconds":MAX_WAIT_SECONDS,"delivery":["event_journal","queued_message_to_notify_session"],
        "message_mode":"queue","message_replays":false,"first_observation":"baseline_only",
        "refusal_codes":["UNSUPPORTED_PROVIDER_OPERATION","STALE_TARGET","INVALID_ARGUMENT","SESSION_NOT_FOUND"]})
}

/// `api turn-notify`: enable (optionally naming the session to message) or disable.
pub fn set(
    paths: &Paths,
    session: &str,
    expected_generation: &str,
    enable: bool,
    notify_session: Option<String>,
) -> Result<Value> {
    let s = catalog::resolve(paths, session, expected_generation)?;
    ensure!(
        s.provider == "codex",
        "UNSUPPORTED_PROVIDER_OPERATION: turn notifications are Codex-only"
    );
    if !enable {
        ensure!(
            notify_session.is_none(),
            "INVALID_ARGUMENT: --notify-session needs --enable"
        );
    }
    if let Some(target) = &notify_session {
        let t = catalog::find(paths, target).context("SESSION_NOT_FOUND: --notify-session")?;
        ensure!(
            t.provider == "codex" && t.id != s.id,
            "INVALID_ARGUMENT: --notify-session must be another Codex session"
        );
    }
    let changed = with_store(paths, |store| {
        if !enable {
            return Ok(match store.sessions.get_mut(&s.id) {
                Some(setting) if setting.enabled => {
                    setting.enabled = false;
                    true
                }
                _ => false,
            });
        }
        let setting = store.sessions.entry(s.id.clone()).or_default();
        let changed = !setting.enabled || setting.notify_session != notify_session;
        if !setting.enabled {
            setting.enabled_at = daemon::now();
            setting.baselined = false;
        }
        setting.enabled = true;
        setting.notify_session = notify_session;
        Ok(changed)
    })?;
    Ok(
        json!({"session_id":s.id,"generation":s.generation(),"changed":changed,"turn_notify":public(all(paths).get(&s.id))}),
    )
}

/// Compare one observation round with the stored state; return the new events.
/// Pure of delivery: the journal gains the events, `delivery` is filled in later.
pub fn process(
    paths: &Paths,
    names: &BTreeMap<String, String>,
    observed: &BTreeMap<String, TurnFacts>,
) -> Result<Vec<Event>> {
    with_store(paths, |store| {
        let mut fresh = Vec::new();
        for (id, setting) in store.sessions.iter_mut().filter(|(_, s)| s.enabled) {
            let Some(facts) = observed.get(id) else {
                continue;
            };
            let state = live_activity::classify(facts);
            if !setting.baselined {
                setting.baselined = true;
            } else {
                let ended = matches!(state, "turn_ended" | "error")
                    && match (&facts.ended_at, &setting.last_ended_at) {
                        (Some(now), Some(before)) => now != before,
                        (Some(_), None) => true,
                        (None, _) => setting.last_state.as_deref() == Some("working"),
                    };
                if ended {
                    fresh.push((id.clone(), state, facts.error.clone()));
                }
            }
            setting.last_state = Some(state.to_owned());
            if facts.ended_at.is_some() {
                setting.last_ended_at = facts.ended_at.clone();
            }
        }
        let mut events = Vec::new();
        for (id, state, error) in fresh {
            store.next_seq += 1;
            let event = Event {
                seq: store.next_seq,
                at: daemon::now(),
                name: names.get(&id).cloned().unwrap_or_default(),
                session_id: id,
                kind: state.to_owned(),
                error: error.filter(|_| state == "error"),
                delivery: None,
            };
            store.events.push(event.clone());
            events.push(event);
        }
        let excess = store.events.len().saturating_sub(MAX_EVENTS);
        store.events.drain(..excess);
        Ok(events)
    })
}

fn record_delivery(paths: &Paths, seq: u64, delivery: Value) {
    let _ = with_store(paths, |store| {
        if let Some(event) = store.events.iter_mut().find(|e| e.seq == seq) {
            event.delivery = Some(delivery);
        }
        Ok(())
    });
}

/// The text queued to the designated session: identity and outcome only.
pub fn message_text(event: &Event) -> String {
    match (&event.kind[..], &event.error) {
        ("error", Some(error)) => format!(
            "[sessionsation] Codex session \"{}\" ({}) ended its turn with an error: {error}",
            event.name, event.session_id
        ),
        ("error", None) => format!(
            "[sessionsation] Codex session \"{}\" ({}) ended its turn with an error.",
            event.name, event.session_id
        ),
        _ => format!(
            "[sessionsation] Codex session \"{}\" ({}) ended its turn.",
            event.name, event.session_id
        ),
    }
}

/// Queue the notification once; the outcome (never a retry) goes in the journal.
async fn deliver(paths: &Paths, event: &Event, target: &str) {
    let outcome = async {
        let generation = catalog::find(paths, target)?.generation();
        let message_id = uuid::Uuid::new_v4().to_string();
        let body = json!({"api_version":1,"message_id":message_id,"session_id":target,
            "expected_generation":generation,"mode":"queue","text":message_text(event),
            "expires_at":daemon::now() + 600});
        let receipt = crate::messaging::headless::dispatch(paths, &serde_json::to_vec(&body)?).await?;
        Ok::<_, anyhow::Error>(json!({"message_id":message_id,"to":target,"state":receipt["state"],"reason":receipt["reason"]}))
    }
    .await
    .unwrap_or_else(|e| json!({"to":target,"state":"failed","error":format!("{e:#}").chars().take(200).collect::<String>()}));
    record_delivery(paths, event.seq, outcome);
}

/// One supervisor round: observe enabled sessions, journal new turn ends, deliver.
pub async fn tick(paths: &Paths) -> Result<Vec<Event>> {
    let store = load(paths)?;
    let enabled: Vec<String> = store
        .sessions
        .iter()
        .filter(|(_, s)| s.enabled)
        .map(|(id, _)| id.clone())
        .collect();
    if enabled.is_empty() {
        return Ok(vec![]);
    }
    let mut names = BTreeMap::new();
    let mut conversations = BTreeMap::new();
    for id in &enabled {
        match catalog::find(paths, id) {
            Ok(s) => {
                names.insert(id.clone(), s.name.clone());
                conversations.insert(s.conversation_id.clone(), id.clone());
            }
            Err(_) => {
                // The session is gone: nothing more to notify about.
                let _ = with_store(paths, |s| {
                    if let Some(setting) = s.sessions.get_mut(id) {
                        setting.enabled = false;
                    }
                    Ok(())
                });
            }
        }
    }
    let live = external::live_sessions(paths).await?;
    let observed: BTreeMap<String, TurnFacts> = live
        .into_iter()
        .filter(|i| !i.inactive)
        .filter_map(|i| {
            let id = conversations.get(&i.thread_id)?.clone();
            Some((id, i.turn_facts?))
        })
        .collect();
    let events = process(paths, &names, &observed)?;
    for event in events.iter().take(MAX_DELIVERIES_PER_TICK) {
        if let Some(target) = store
            .sessions
            .get(&event.session_id)
            .and_then(|s| s.notify_session.clone())
        {
            deliver(paths, event, &target).await;
        }
    }
    Ok(events)
}

/// `api turn-events`: events after `after` (0 for all kept), waiting up to `wait`
/// seconds for the first one. `cursor` is what to pass as the next `after`.
pub async fn events(paths: &Paths, after: u64, wait: u64) -> Result<Value> {
    ensure!(
        wait <= MAX_WAIT_SECONDS,
        "INVALID_ARGUMENT: --wait is at most {MAX_WAIT_SECONDS} seconds"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let store = load(paths)?;
        let oldest = store.events.first().map_or(store.next_seq + 1, |e| e.seq);
        let gap = after + 1 < oldest && store.next_seq > after;
        let events: Vec<&Event> = store.events.iter().filter(|e| e.seq > after).collect();
        if !events.is_empty() || std::time::Instant::now() >= deadline {
            let cursor = events.last().map_or(after, |e| e.seq);
            return Ok(json!({"events":events,"cursor":cursor,"gap":gap,"waited":wait>0}));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The supervisor loop (`SESSIONSATION_TURN_NOTIFY_SECONDS`, default 5; 0 disables).
pub async fn monitor(paths: Paths) {
    let every = std::env::var("SESSIONSATION_TURN_NOTIFY_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(POLL_SECONDS);
    if every == 0 {
        return;
    }
    loop {
        let _ = tick(&paths).await;
        tokio::time::sleep(Duration::from_secs(every)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("state"),
            runtime: dir.path().join("runtime"),
        };
        fs::create_dir_all(&paths.root).unwrap();
        (dir, paths)
    }
    fn enable(paths: &Paths, id: &str, to: Option<&str>) {
        with_store(paths, |s| {
            s.sessions.insert(
                id.into(),
                Setting {
                    enabled: true,
                    notify_session: to.map(str::to_owned),
                    enabled_at: 1,
                    ..Default::default()
                },
            );
            Ok(())
        })
        .unwrap();
    }
    fn facts(events: &[Value]) -> TurnFacts {
        live_activity::facts_from_events(events)
    }
    fn ev(kind: &str, at: &str, message: Option<&str>) -> Value {
        json!({"timestamp":at,"type":"event_msg","payload":{"type":kind,"message":message}})
    }
    fn round(paths: &Paths, id: &str, f: TurnFacts) -> Vec<Event> {
        let names = BTreeMap::from([(id.to_owned(), "worker".to_owned())]);
        process(paths, &names, &BTreeMap::from([(id.to_owned(), f)])).unwrap()
    }

    #[test]
    fn each_turn_end_is_reported_once_and_the_first_look_is_only_a_baseline() {
        let (_d, paths) = paths();
        let id = "s1";
        enable(&paths, id, None);
        let started = ev("task_started", "t1", None);
        let done = ev("task_complete", "t2", None);
        // Already finished before notifications were enabled: not an event.
        assert!(round(&paths, id, facts(&[started.clone(), done.clone()])).is_empty());
        assert!(round(&paths, id, facts(&[started.clone(), done.clone()])).is_empty());
        // Working, then ended: one event; polling again adds none.
        assert!(
            round(
                &paths,
                id,
                facts(&[
                    started.clone(),
                    done.clone(),
                    ev("task_started", "t3", None)
                ])
            )
            .is_empty()
        );
        let both = [
            started.clone(),
            done.clone(),
            ev("task_started", "t3", None),
            ev("task_complete", "t4", None),
        ];
        let got = round(&paths, id, facts(&both));
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].kind.as_str(), got[0].seq, got[0].name.as_str()),
            ("turn_ended", 1, "worker")
        );
        assert!(round(&paths, id, facts(&both)).is_empty());
        // A turn shorter than the poll interval (never seen working) still counts once.
        let quick = [
            started.clone(),
            done.clone(),
            ev("task_started", "t5", None),
            ev("task_complete", "t6", None),
        ];
        assert_eq!(round(&paths, id, facts(&quick)).len(), 1);
        // A failed turn reports the provider's error text.
        let failed = [
            ev("task_started", "t7", None),
            ev("error", "t7", Some("Selected model is at capacity")),
            ev("task_complete", "t8", None),
        ];
        let got = round(&paths, id, facts(&failed));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "error");
        assert_eq!(
            got[0].error.as_deref(),
            Some("Selected model is at capacity")
        );
        assert!(message_text(&got[0]).contains("with an error: Selected model is at capacity"));
        // A disabled session and an unobserved (not live) one never emit.
        with_store(&paths, |s| {
            s.sessions.get_mut(id).unwrap().enabled = false;
            Ok(())
        })
        .unwrap();
        assert!(round(&paths, id, facts(&both)).is_empty());
    }

    #[test]
    fn without_timestamps_only_a_seen_working_to_ended_transition_counts() {
        let (_d, paths) = paths();
        enable(&paths, "s1", None);
        let plain = |kind: &str| json!({"type":"event_msg","payload":{"type":kind}});
        assert!(
            round(
                &paths,
                "s1",
                facts(&[plain("task_started"), plain("task_complete")])
            )
            .is_empty()
        );
        assert!(round(&paths, "s1", facts(&[plain("task_started")])).is_empty());
        assert_eq!(
            round(
                &paths,
                "s1",
                facts(&[plain("task_started"), plain("task_complete")])
            )
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn waiters_get_events_after_a_cursor_and_see_gaps() {
        let (_d, paths) = paths();
        enable(&paths, "s1", Some("other"));
        let t =
            |a: &str, b: &str| facts(&[ev("task_started", a, None), ev("task_complete", b, None)]);
        round(&paths, "s1", t("a", "b"));
        round(&paths, "s1", t("c", "d"));
        round(&paths, "s1", t("e", "f"));
        let all = events(&paths, 0, 0).await.unwrap();
        assert_eq!(all["events"].as_array().unwrap().len(), 2);
        assert_eq!(all["cursor"], 2);
        let none = events(&paths, 2, 0).await.unwrap();
        assert!(none["events"].as_array().unwrap().is_empty());
        assert_eq!(none["cursor"], 2);
        assert!(events(&paths, 0, MAX_WAIT_SECONDS + 1).await.is_err());
        // A waiter blocked on the cursor wakes when a new turn ends.
        let writer = {
            let paths = paths.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                round(&paths, "s1", t("g", "h"));
            })
        };
        let woke = events(&paths, 2, 5).await.unwrap();
        writer.await.unwrap();
        assert_eq!(woke["events"][0]["seq"], 3);
        // Dropped history is flagged.
        with_store(&paths, |s| {
            s.events.drain(..2);
            Ok(())
        })
        .unwrap();
        assert_eq!(events(&paths, 0, 0).await.unwrap()["gap"], true);
    }

    #[test]
    fn the_journal_is_bounded() {
        let (_d, paths) = paths();
        with_store(&paths, |s| {
            for seq in 1..=(MAX_EVENTS as u64 + 5) {
                s.events.push(Event {
                    seq,
                    at: 0,
                    session_id: "x".into(),
                    name: String::new(),
                    kind: "turn_ended".into(),
                    error: None,
                    delivery: None,
                });
            }
            s.next_seq = MAX_EVENTS as u64 + 5;
            Ok(())
        })
        .unwrap();
        enable(&paths, "s1", None);
        let t =
            |a: &str, b: &str| facts(&[ev("task_started", a, None), ev("task_complete", b, None)]);
        round(&paths, "s1", t("a", "b"));
        round(&paths, "s1", t("c", "d"));
        assert_eq!(load(&paths).unwrap().events.len(), MAX_EVENTS);
    }
}
