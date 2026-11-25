use super::*;

#[test]
fn claude_ids_are_exact_and_aliases_are_refused() {
    for ok in [
        "claude-fable-5-2",
        "claude-opus-5-5",
        "claude-opus-5-5[1m]",
        "claude-haiku-4.5",
    ] {
        validate_claude_model(ok).unwrap();
    }
    for bad in [
        "fable",
        "opus",
        "sonnet",
        "haiku",
        "default",
        "opusplan",
        "best",
        "Claude-Opus-5-5",
        "claude-",
        "claude--x",
        "claude-opus 5",
        "-x",
        "claude-opus-5-5[2m]",
    ] {
        assert!(validate_claude_model(bad).is_err(), "{bad} must be refused");
    }
    validate(ProviderId::Claude, "claude-opus-5-5", Some("xhigh")).unwrap();
    assert!(validate(ProviderId::Claude, "claude-opus-5-5", Some("minimal")).is_err());
    validate(ProviderId::Codex, "gpt-6-astra", Some("minimal")).unwrap();
    assert!(validate(ProviderId::Codex, "gpt-6-astra", Some("max")).is_err());
    assert!(
        validate(ProviderId::Gemini, "x", None)
            .unwrap_err()
            .to_string()
            .starts_with("UNSUPPORTED_PROVIDER_OPERATION")
    );
}

#[test]
fn hook_records_model_and_effort_and_bounds_rewrites() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let id = "11111111-1111-4111-8111-111111111111";
    let payload = json!({"session_id":id,"version":"2.1.314","model":{"id":"claude-fable-5-2","display_name":"Fable 5.2"},
        "effort":{"level":"high"},"thinking":{"enabled":true},"fast_mode":false});
    assert!(record_hook(home, &payload, 1_000_000).unwrap());
    let r = hook_record(home, id).unwrap();
    assert_eq!(r.model_id.as_deref(), Some("claude-fable-5-2"));
    assert_eq!(r.effort_level.as_deref(), Some("high"));
    assert_eq!(r.claude_version.as_deref(), Some("2.1.314"));
    // Unchanged within a minute: not rewritten.
    assert!(!record_hook(home, &payload, 1_010_000).unwrap());
    // Unchanged after a minute: rewritten, changed_at kept.
    assert!(record_hook(home, &payload, 1_070_000).unwrap());
    let r = hook_record(home, id).unwrap();
    assert_eq!((r.observed_at_ms, r.changed_at_ms), (1_070_000, 1_000_000));
    // No effort (a model without effort support) is null.
    let mut no_effort = payload.clone();
    no_effort.as_object_mut().unwrap().remove("effort");
    assert!(record_hook(home, &no_effort, 1_080_000).unwrap());
    assert_eq!(hook_record(home, id).unwrap().effort_level, None);
    // No model: the previous record stays; a non-UUID id writes nothing.
    assert!(!record_hook(home, &json!({"session_id":id}), 1_090_000).unwrap());
    assert!(hook_record(home, id).is_some());
    assert!(
        !record_hook(
            home,
            &json!({"session_id":"../x","model":{"id":"claude-a-b"}}),
            1
        )
        .unwrap()
    );
    assert!(hook_file(home, "../x").is_none());
}

#[test]
fn current_prefers_the_newer_observation_and_rows_get_the_field() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let home = dir.path().join("home");
    let id = "22222222-2222-4222-8222-222222222222";
    assert!(current(&paths, id, Some(&home)).is_none());
    store(
        &paths,
        id,
        Observed {
            model: "claude-opus-5-5".into(),
            effort: Some("medium".into()),
            observed_at: 100,
            source: "set_model".into(),
        },
    )
    .unwrap();
    assert_eq!(current(&paths, id, None).unwrap().model, "claude-opus-5-5");
    record_hook(
        &home,
        &json!({"session_id":id,"model":{"id":"claude-fable-5-2"},"effort":{"level":"high"}}),
        200_000,
    )
    .unwrap();
    let now = current(&paths, id, Some(&home)).unwrap();
    assert_eq!(
        (now.model.as_str(), now.source.as_str()),
        ("claude-fable-5-2", "claude_statusline")
    );
    let mut rows = vec![
        json!({"provider":"codex","conversation_id":id,"account":"a"}),
        json!({"provider":"gemini","conversation_id":id,"account":"a"}),
    ];
    annotate(&paths, &mut rows);
    assert_eq!(rows[0]["model"]["id"], "claude-opus-5-5");
    assert_eq!(rows[0]["model"]["source"], "set_model");
    assert!(rows[1]["model"].is_null());
    record_codex_threads(
        &paths,
        &[json!({"id":id,"model":"gpt-6-astra","reasoningEffort":"high"})],
    );
    let c = current(&paths, id, None).unwrap();
    assert_eq!(
        (c.model.as_str(), c.effort.as_deref()),
        ("gpt-6-astra", Some("high"))
    );
}
