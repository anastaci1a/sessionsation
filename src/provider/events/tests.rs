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
fn facts(state: &str, ended_at: Option<&str>) -> TurnFacts {
    TurnFacts {
        observed: true,
        started: state != "idle",
        in_progress: state == "working",
        last_status: (state == "ended").then(|| "completed".into()),
        ended_at: ended_at.map(str::to_owned),
        ..Default::default()
    }
}
fn round(provider: ProviderId, state: &str, ended_at: Option<&str>) -> Turns {
    Turns {
        session_id: "s1".into(),
        provider,
        conversation_id: "c1".into(),
        runtime: Some(100),
        facts: facts(state, ended_at),
    }
}
fn keys(v: &Value) -> Vec<String> {
    v.as_object().unwrap().keys().cloned().collect()
}

#[tokio::test]
async fn claude_hooks_and_codex_turns_come_out_in_one_shape() {
    let (_d, paths) = paths();
    // Codex: the first look is a baseline, then a start and an end.
    observe_turns(&paths, &[round(ProviderId::Codex, "ended", Some("t1"))]).unwrap();
    observe_turns(&paths, &[round(ProviderId::Codex, "working", Some("t1"))]).unwrap();
    observe_turns(&paths, &[round(ProviderId::Codex, "ended", Some("t2"))]).unwrap();
    // A turn shorter than the refresh is still reported once; a repeat adds nothing.
    observe_turns(&paths, &[round(ProviderId::Codex, "ended", Some("t3"))]).unwrap();
    observe_turns(&paths, &[round(ProviderId::Codex, "ended", Some("t3"))]).unwrap();
    // Gemini produces nothing; Claude rows are hook-sourced, not supervisor-observed.
    observe_turns(&paths, &[round(ProviderId::Gemini, "working", None)]).unwrap();
    // Claude: hook lines in the account home, mapped by the same journal.
    let mut state = State::default();
    let home = paths.root.join("claude-home");
    fs::create_dir_all(home.join(".sessionsation")).unwrap();
    let lines = [
        json!({"hook_event_name":"SessionStart","session_id":"conv","source":"startup"}),
        json!({"hook_event_name":"UserPromptSubmit","session_id":"conv","prompt":"secret prompt"}),
        json!({"hook_event_name":"SubagentStart","session_id":"conv","agent_id":"a1","agent_type":"Explore"}),
        json!({"hook_event_name":"PostToolUse","session_id":"conv","tool_name":"Bash","tool_input":{"command":"secret"},"tool_response":{"backgroundTaskId":"b1"}}),
        json!({"hook_event_name":"PostToolUse","session_id":"conv","tool_name":"Bash","tool_response":{"stdout":"x"}}),
        json!({"hook_event_name":"Stop","session_id":"conv","last_assistant_message":"secret reply"}),
    ];
    let text: String = lines
        .iter()
        .map(|l| hook_line(l, 1_800_000_000_000).unwrap().to_string() + "\n")
        .collect();
    fs::write(hook_file(&home), &text).unwrap();
    with_state(&paths, |s, fresh| {
        ingest_file(&paths, s, &home, fresh);
        state = s.clone();
    })
    .unwrap();
    let all = read(
        &paths,
        Query {
            after: 0,
            wait: 0,
            session: None,
            kind: None,
            limit: None,
        },
    )
    .await
    .unwrap();
    let events = all["events"].as_array().unwrap();
    let kinds: Vec<(&str, &str)> = events
        .iter()
        .map(|e| (e["provider"].as_str().unwrap(), e["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("codex", "turn_started"),
            ("codex", "turn_completed"),
            ("codex", "turn_completed"),
            ("claude", "session_started"),
            ("claude", "turn_started"),
            ("claude", "subagent_started"),
            ("claude", "background_task_started"),
            ("claude", "turn_completed"),
        ]
    );
    for e in events {
        assert_eq!(keys(e), keys(&events[0]), "{e}");
        assert!(KINDS.contains(&e["kind"].as_str().unwrap()));
    }
    assert_eq!(events[0]["source"], "supervisor");
    assert_eq!(events[0]["session_id"], "s1");
    assert_eq!(events[3]["source"], "hook");
    assert_eq!(events[3]["provider_conversation_id"], "conv");
    assert_eq!(events[3]["at"], 1_800_000_000);
    assert_eq!(events[5]["detail"]["agent_type"], "Explore");
    assert_eq!(events[6]["detail"]["task_id"], "b1");
    assert!(!all.to_string().contains("secret"), "{all}");
    assert_eq!(
        (all["cursor"].as_u64(), all["gap"].as_bool()),
        (Some(8), Some(false))
    );
    // The tracker counts what is running.
    assert_eq!(state.tasks["conv"].subagents.len(), 1);
    assert_eq!(state.tasks["conv"].background.len(), 1);
    // Filters and the cursor.
    let codex_ends = read(
        &paths,
        Query {
            after: 0,
            wait: 0,
            session: Some("s1"),
            kind: Some("turn_completed"),
            limit: Some(1),
        },
    )
    .await
    .unwrap();
    assert_eq!(codex_ends["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        (codex_ends["more"].as_bool(), codex_ends["cursor"].as_u64()),
        (Some(true), Some(2))
    );
    let none = read(
        &paths,
        Query {
            after: 8,
            wait: 0,
            session: None,
            kind: None,
            limit: None,
        },
    )
    .await
    .unwrap();
    assert!(none["events"].as_array().unwrap().is_empty());
    assert_eq!(
        (none["cursor"].as_u64(), none["gap"].as_bool()),
        (Some(8), Some(false))
    );
    // A line already ingested is never journaled twice.
    with_state(&paths, |s, fresh| {
        ingest_file(&paths, s, &home, fresh);
        assert!(fresh.is_empty());
    })
    .unwrap();
}

#[tokio::test]
async fn dropped_events_are_flagged_as_a_gap() {
    let (_d, paths) = paths();
    with_state(&paths, |s, fresh| {
        s.observed.clear();
        for _ in 0..(MAX_EVENTS + 1) {
            fresh.push(event(
                ProviderId::Codex,
                None,
                None,
                "turn_started",
                1,
                "supervisor",
                json!({}),
            ));
        }
    })
    .unwrap();
    let state = load(&paths);
    assert_eq!(state.kept, KEEP);
    assert_eq!(state.latest_seq, MAX_EVENTS as u64 + 1);
    assert_eq!(state.oldest_seq, state.latest_seq - KEEP as u64 + 1);
    assert_eq!(read_all(&paths).len(), KEEP);
    let q = |after| Query {
        after,
        wait: 0,
        session: None,
        kind: None,
        limit: Some(1),
    };
    let old = read(&paths, q(0)).await.unwrap();
    assert_eq!(old["gap"], true);
    assert_eq!(old["events"][0]["seq"], state.oldest_seq);
    // A reader already past the dropped range sees no gap.
    let current = read(&paths, q(state.oldest_seq)).await.unwrap();
    assert_eq!(current["gap"], false);
    assert!(
        read(
            &paths,
            Query {
                wait: MAX_WAIT_SECONDS + 1,
                ..q(0)
            }
        )
        .await
        .is_err()
    );
    assert!(
        read(
            &paths,
            Query {
                kind: Some("nope"),
                ..q(0)
            }
        )
        .await
        .is_err()
    );
    // A waiter wakes when an event arrives.
    let writer = {
        let paths = paths.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            record(
                &paths,
                ProviderId::Codex,
                "s9",
                "c9",
                "compacted",
                json!({}),
            )
            .unwrap();
        })
    };
    let latest = state.latest_seq;
    let woke = read(
        &paths,
        Query {
            wait: 5,
            ..q(latest)
        },
    )
    .await
    .unwrap();
    writer.await.unwrap();
    assert_eq!(woke["events"][0]["kind"], "compacted");
    assert_eq!(woke["events"][0]["source"], "operation");
}

#[test]
fn a_crash_after_the_journal_append_never_reuses_a_number() {
    let (_d, paths) = paths();
    record(&paths, ProviderId::Codex, "s", "c", "compacted", json!({})).unwrap();
    // The state write after the append was lost.
    fs::remove_file(state_file(&paths)).unwrap();
    record(&paths, ProviderId::Codex, "s", "c", "compacted", json!({})).unwrap();
    let seqs: Vec<u64> = read_all(&paths).iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2]);
}

#[test]
fn subagents_are_counted_from_start_and_stop_events_in_any_order() {
    let mut t = Tasks::default();
    let ev = |id: &str| json!({"agent_id":id,"agent_type":"general-purpose"});
    track(&mut t, "session_started", &json!({"source":"startup"}), 10);
    track(&mut t, "subagent_started", &ev("a"), 11);
    track(&mut t, "subagent_started", &ev("b"), 11);
    track(&mut t, "subagent_stopped", &ev("a"), 12);
    assert_eq!(t.subagents.len(), 1);
    // A stop journaled before its start (async hooks) cancels it.
    track(&mut t, "subagent_stopped", &ev("c"), 13);
    track(&mut t, "subagent_started", &ev("c"), 13);
    assert_eq!(t.subagents.keys().collect::<Vec<_>>(), ["b"]);
    // Compaction keeps the process; a resume starts over; an end clears.
    track(&mut t, "session_started", &json!({"source":"compact"}), 14);
    assert_eq!(t.subagents.len(), 1);
    assert_eq!(t.process_started_at, Some(10));
    track(&mut t, "session_started", &json!({"source":"resume"}), 20);
    assert!(t.subagents.is_empty());
    assert_eq!(t.process_started_at, Some(20));
    track(&mut t, "subagent_started", &ev("d"), 21);
    track(&mut t, "session_ended", &json!({"reason":"exit"}), 22);
    assert!(t.subagents.is_empty() && t.ended);
}

#[test]
fn a_new_runtime_is_a_baseline_and_agy_transitions_are_turns() {
    let mut s = State::default();
    let agy = |state: &str, runtime| Turns {
        runtime: Some(runtime),
        ..round(ProviderId::Antigravity, state, None)
    };
    assert!(compare(&mut s, &[agy("working", 1)], 1).is_empty());
    let ended = compare(&mut s, &[agy("ended", 1)], 2);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].kind, "turn_completed");
    assert_eq!(ended[0].detail["status"], "completed");
    // A restarted process: no event for what it shows first.
    assert!(compare(&mut s, &[agy("working", 2)], 3).is_empty());
    assert_eq!(compare(&mut s, &[agy("ended", 2)], 4).len(), 1);
}
