use super::*;

fn paths() -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    std::fs::create_dir_all(&paths.root).unwrap();
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
    let t = |a: &str, b: &str| facts(&[ev("task_started", a, None), ev("task_complete", b, None)]);
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
                detail: None,
                delivery: None,
            });
        }
        s.next_seq = MAX_EVENTS as u64 + 5;
        Ok(())
    })
    .unwrap();
    enable(&paths, "s1", None);
    let t = |a: &str, b: &str| facts(&[ev("task_started", a, None), ev("task_complete", b, None)]);
    round(&paths, "s1", t("a", "b"));
    round(&paths, "s1", t("c", "d"));
    assert_eq!(load(&paths).unwrap().events.len(), MAX_EVENTS);
}
