use super::*;
fn req(mode: &str) -> Value {
    json!({"api_version":1,"message_id":"m","session_id":"s","expected_generation":"g",
        "mode":mode,"text":"t","expires_at":1})
}
#[test]
fn intents_map_per_provider_and_other_modes_are_untouched() {
    let mut v = req("deliver");
    let p = plan(Some(ProviderId::Codex), &mut v).unwrap();
    assert_eq!(
        (v["mode"].as_str(), v["if_busy"].as_str()),
        (Some("submit"), Some("queue"))
    );
    assert_eq!(p.intent(), Some("deliver"));
    assert!(p.ignored_options.is_empty() && p.ignored_reason.is_none());
    for provider in [
        ProviderId::Claude,
        ProviderId::Antigravity,
        ProviderId::Gemini,
    ] {
        for (intent, mode) in [
            ("deliver", "submit"),
            ("when_idle", "queue"),
            ("interrupt", "steer"),
        ] {
            let mut v = req(intent);
            plan(Some(provider), &mut v).unwrap();
            assert_eq!(v["mode"], mode);
            assert!(v.get("if_busy").is_none());
        }
    }
    let mut v = req("when_idle");
    plan(None, &mut v).unwrap();
    assert_eq!(
        v["mode"], "queue",
        "an unknown target follows the Codex contract"
    );
    for mode in ["resume", "new", "bogus"] {
        let mut v = req(mode);
        let before = v.clone();
        assert!(plan(Some(ProviderId::Claude), &mut v).is_none());
        assert_eq!(v, before);
    }
}
#[test]
fn codex_only_options_to_claude_are_ignored_and_named() {
    let mut v = req("interrupt");
    v["if_busy"] = json!("queue");
    v["recovery"] = json!({"version":1});
    v["steering"] = json!({"version":1,"expected_turn_id":"t","expected_runtime_generation":"g"});
    v["timeout_seconds"] = json!(30);
    let p = plan(Some(ProviderId::Claude), &mut v).unwrap();
    assert_eq!(
        p.ignored_options,
        [
            "if_busy",
            "recovery",
            "steering.expected_turn_id",
            "steering.expected_runtime_generation"
        ]
    );
    assert!(
        p.ignored_reason
            .as_deref()
            .unwrap()
            .starts_with("Claude sessions do not use if_busy")
    );
    assert_eq!(v["steering"], json!({"version":1}));
    assert_eq!(v["timeout_seconds"], 30, "applies to Claude");
    assert!(v.get("if_busy").is_none() && v.get("recovery").is_none());
}
#[test]
fn claude_only_options_to_codex_and_antigravity_are_ignored() {
    let mut v = req("submit");
    v["delivery"] = json!("peer_message");
    v["await_reply"] = json!(true);
    v["timeout_seconds"] = json!(30);
    v["steering"] = json!({"version":1,"interrupt":true});
    let p = plan(Some(ProviderId::Codex), &mut v).unwrap();
    assert_eq!(
        p.ignored_options,
        [
            "timeout_seconds",
            "delivery",
            "await_reply",
            "steering.interrupt"
        ]
    );
    assert!(v.get("delivery").is_none() && v.get("steering").is_none());
    assert_eq!(v["await_reply"], true, "kept: the Codex request accepts it");
    let mut v = req("steer");
    v["delivery"] = json!("terminal_input");
    v["steering"] = json!({"version":1,"interrupt":false});
    let p = plan(Some(ProviderId::Antigravity), &mut v).unwrap();
    assert_eq!(p.ignored_options, ["steering.interrupt"]);
    assert_eq!(v["delivery"], "terminal_input", "applies to typed input");
    assert_eq!(v["steering"], json!({"version":1}));
    // Unknown values are still the provider's to refuse.
    let mut v = req("submit");
    v["delivery"] = json!("carrier_pigeon");
    assert!(
        plan(Some(ProviderId::Codex), &mut v)
            .unwrap()
            .ignored_options
            .is_empty()
    );
    assert_eq!(v["delivery"], "carrier_pigeon");
}
#[test]
fn capability_lists_every_provider() {
    let c = capability();
    assert_eq!(
        c["providers"]["codex"]["intents"]["deliver"]["if_busy"],
        "queue"
    );
    assert_eq!(
        c["providers"]["claude"]["intents"]["when_idle"]["mode"],
        "queue"
    );
    let ignored = c["providers"]["claude"]["ignored_options"]
        .as_array()
        .unwrap();
    assert!(ignored.contains(&json!("if_busy")) && !ignored.contains(&json!("delivery")));
}
