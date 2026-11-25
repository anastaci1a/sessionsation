use super::*;

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn every_provider_row_gets_the_same_keys_with_sources_or_reasons() {
    let t = tempfile::tempdir().unwrap();
    let rollout = t.path().join("rollout.jsonl");
    let event = |tokens: u64, at: &str| {
        json!({"timestamp":at,"type":"event_msg","payload":{"type":"token_count","info":{
            "total_token_usage":{"total_tokens":tokens * 3},
            "last_token_usage":{"input_tokens":tokens - 100,"output_tokens":100,"total_tokens":tokens},
            "model_context_window":258_400},"rate_limits":null}})
        .to_string()
    };
    let mut text = String::new();
    text.push_str(&event(10_000, "2026-11-23T10:00:00.000Z"));
    text.push('\n');
    // Codex emits token_count with info null before the first response.
    text.push_str(r#"{"timestamp":"2026-11-23T10:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#);
    text.push('\n');
    text.push_str(&event(64_600, "2026-11-23T10:05:00.500Z"));
    text.push('\n');
    // A large tool output after the newest count still leaves it in the bounded tail.
    text.push_str(&format!(
        "{{\"type\":\"response_item\",\"payload\":{{\"output\":\"{}\"}}}}\n",
        "x".repeat(100_000)
    ));
    fs::write(&rollout, text).unwrap();
    let c = codex_context(&rollout).unwrap();
    assert_eq!((c.pct, c.tokens, c.window), (25.0, 64_600, 258_400));
    assert_eq!(
        c.observed_at,
        crate::reset_credit::claude::parse_time("2026-11-23T10:05:00Z")
    );

    let mut codex = json!({"provider":"codex"});
    apply_codex(&mut codex, Some(&rollout), true);
    assert_eq!(codex["context_pct"], 25.0);
    assert_eq!(codex["context_pct_source"], "rollout");
    assert!(codex["context_pct_reason"].is_null());
    assert!(codex["subagents_running"].is_null());
    assert_eq!(codex["subagents_running_reason"], CODEX_TASKS);
    assert!(codex["subagents_running_source"].is_null());
    // A cached row keeps its reading.
    let before = codex.clone();
    apply(&mut codex, None, None);
    assert_eq!(codex, before);
    let mut stopped = json!({"provider":"codex"});
    apply_codex(&mut stopped, Some(&rollout), false);
    assert!(stopped["context_pct"].is_null());
    assert_eq!(stopped["context_pct_reason"], CODEX_NOT_RUNNING);
    let empty = t.path().join("empty.jsonl");
    fs::write(&empty, "{\"type\":\"session_meta\"}\n").unwrap();
    let mut fresh = json!({"provider":"codex"});
    apply_codex(&mut fresh, Some(&empty), true);
    assert_eq!(fresh["context_pct_reason"], CODEX_NO_TOKEN_COUNT);

    let hook = crate::session_model::HookRecord {
        schema_version: 1,
        session_id: "s".into(),
        model_id: Some("claude-fable-5-2".into()),
        model_display_name: None,
        effort_level: None,
        thinking_enabled: None,
        fast_mode: None,
        claude_version: None,
        observed_at_ms: 1_800_000_000_123,
        changed_at_ms: 1,
        context_used_percentage: Some(42.0),
        context_tokens: Some(84_000),
        context_window_size: Some(200_000),
    };
    let mut claude = json!({"provider":"claude"});
    apply(&mut claude, Some(&hook), None);
    assert_eq!(claude["context_pct"], 42.0);
    assert_eq!(claude["context_pct_source"], "statusline");
    assert_eq!(claude["context_pct_observed_at"], 1_800_000_000);
    assert_eq!(claude["context_window_size"], 200_000);
    assert_eq!(claude["background_tasks_reason"], CLAUDE_BACKGROUND);
    assert_eq!(claude["subagents_running_reason"], CLAUDE_NOT_RUNNING);
    let mut unread = json!({"provider":"claude"});
    apply(
        &mut unread,
        Some(&crate::session_model::HookRecord {
            context_used_percentage: None,
            ..hook.clone()
        }),
        None,
    );
    assert!(unread["context_pct"].is_null());
    assert_eq!(unread["context_pct_reason"], CLAUDE_NO_CONTEXT_YET);
    let mut gemini = json!({"provider":"gemini"});
    apply(&mut gemini, None, None);
    assert_eq!(gemini["context_pct_reason"], GEMINI_CONTEXT);
    let mut agy = json!({"provider":"antigravity"});
    apply(&mut agy, None, None);
    for row in [&claude, &unread, &gemini, &agy, &stopped, &fresh] {
        assert_eq!(keys(row), keys(&codex), "{row}");
    }
}

#[test]
fn claude_subagents_running_is_counted_from_the_hook_events_of_this_process() {
    use crate::provider::events::Tasks;
    let mut t = Tasks {
        process_started_at: Some(1_000),
        last_event_at: 1_050,
        ..Default::default()
    };
    t.subagents.insert("a1".into(), "Explore".into());
    t.subagents.insert("a2".into(), "general-purpose".into());
    let row = |active_since: i64| json!({"provider":"claude","availability":"running","active_since":active_since});
    let mut running = row(999);
    apply(&mut running, None, Some(&t));
    assert_eq!(running["subagents_running"], 2);
    assert_eq!(running["subagents_running_source"], "hook_events");
    assert_eq!(running["subagents_running_observed_at"], 1_050);
    assert!(running["subagents_running_reason"].is_null());
    assert!(running["background_tasks"].is_null());
    assert_eq!(running["background_tasks_reason"], CLAUDE_BACKGROUND);
    // None running is a real 0.
    let idle = Tasks {
        subagents: Default::default(),
        ..t.clone()
    };
    let mut zero = row(999);
    apply(&mut zero, None, Some(&idle));
    assert_eq!(zero["subagents_running"], 0);
    // A tracker from an earlier process (started before this one), an ended one, none at
    // all, or a stopped session: null with the reason.
    for (r, tasks, reason) in [
        (row(2_000), Some(&t), CLAUDE_NO_HOOK_EVENTS),
        (row(999), None, CLAUDE_NO_HOOK_EVENTS),
        (
            json!({"provider":"claude","availability":"saved"}),
            Some(&t),
            CLAUDE_NOT_RUNNING,
        ),
    ] {
        let mut r = r;
        apply(&mut r, None, tasks);
        assert!(r["subagents_running"].is_null(), "{r}");
        assert_eq!(r["subagents_running_reason"], reason);
    }
    let ended = Tasks {
        ended: true,
        ..t.clone()
    };
    let mut r = row(999);
    apply(&mut r, None, Some(&ended));
    assert_eq!(r["subagents_running_reason"], CLAUDE_NO_HOOK_EVENTS);
}
