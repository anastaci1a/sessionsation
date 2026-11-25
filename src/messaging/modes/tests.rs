use super::*;
fn fixtures() -> Value {
    serde_json::from_str(include_str!("../../../tests/fixtures/legacy-modes.json")).unwrap()
}
fn run(v: Value) -> Result<Value> {
    engine::fixture(serde_json::from_value(v).unwrap())
}
fn request() -> Value {
    fixtures()["request"].clone()
}
fn snapshot() -> Value {
    fixtures()["snapshot"].clone()
}
fn scenario(steps: Vec<Value>) -> Value {
    json!({"now":2000000000,"authenticated":true,"csrf_valid":true,"origin":ORIGIN,"sending_enabled":true,"steps":steps})
}
fn accept(r: Value) -> Value {
    json!({"op":"accept","request":r})
}
fn dispatch(id: Value, s: Value, result: &str) -> Value {
    json!({"op":"dispatch","request_id":id,"snapshot":s,"native_result":result})
}
#[test]
fn idle_submit_and_busy_steer_are_explicit_and_not_consumed_on_ack() {
    for busy in [false, true] {
        let mut r = request();
        let mut s = snapshot();
        if busy {
            s["state"] = json!("busy");
            s["turn_id"] = json!("turn-a");
            r["expected_turn_id"] = json!("turn-a");
        }
        let v = run(scenario(vec![
            accept(r.clone()),
            dispatch(r["request_id"].clone(), s, "ack"),
        ]))
        .unwrap();
        let out = &v["results"][1]["result"];
        assert_eq!(
            out["mock_call"]["method"],
            if busy { "turn/steer" } else { "turn/start" }
        );
        assert_eq!(
            out["receipt"]["effective_action"],
            if busy { "steer" } else { "start" }
        );
        assert_eq!(out["receipt"]["state"], "native_accepted");
        assert!(!out["receipt"]["facts"].to_string().contains("consumed"));
    }
}
#[test]
fn mode_target_payload_preconditions_and_creation_are_immutable() {
    for (key, val) in [
        ("mode", json!("queue")),
        ("session_id", json!("33333333-3333-4333-8333-333333333333")),
        ("text", json!("other")),
        ("expected_turn_id", json!("turn-a")),
        ("created_at", json!(1999999999)),
        ("capability_id", json!("other")),
    ] {
        let r = request();
        let mut changed = r.clone();
        changed[key] = val;
        let v = scenario(vec![
            accept(r.clone()),
            json!({"op":"restart"}),
            accept(r),
            accept(changed),
        ]);
        let v = run(v).unwrap();
        assert_eq!(v["results"][0], v["results"][2]);
        assert_eq!(v["results"][3]["error"]["code"], "idempotency_conflict");
    }
}
#[test]
fn races_stale_evidence_and_ineligible_states_never_fallback() {
    for (field, val, code) in [
        ("generation", json!("b".repeat(64)), "generation_mismatch"),
        ("build", json!("0.999.0"), "capability_changed"),
        ("capability_id", json!("changed"), "capability_changed"),
        (
            "capability_expires_at",
            json!(2000000000),
            "capability_changed",
        ),
        ("observed_at", json!(1999999900), "state_unverified"),
        ("modal", json!(true), "state_unverified"),
        ("state", json!("paused"), "target_ineligible"),
        ("state", json!("stopped"), "target_ineligible"),
        ("state", json!("disconnected"), "target_ineligible"),
        ("state", json!("unknown"), "target_ineligible"),
    ] {
        let r = request();
        let mut s = snapshot();
        s[field] = val;
        let v = run(scenario(vec![
            accept(r.clone()),
            dispatch(r["request_id"].clone(), s, "ack"),
        ]))
        .unwrap();
        assert_eq!(v["results"][1]["result"]["receipt"]["reason"], code);
        assert!(v["results"][1]["result"]["mock_call"].is_null());
    }
    let mut r = request();
    r["expected_turn_id"] = json!("finished-turn");
    let v = run(scenario(vec![
        accept(r.clone()),
        dispatch(r["request_id"].clone(), snapshot(), "ack"),
    ]))
    .unwrap();
    assert_eq!(
        v["results"][1]["result"]["receipt"]["reason"],
        "turn_conflict"
    );
}
#[test]
fn timeout_and_crash_intent_never_redispatch_after_restart() {
    for result in ["timeout", "crash_before_ack", "turn_conflict"] {
        let r = request();
        let v = run(scenario(vec![
            accept(r.clone()),
            dispatch(r["request_id"].clone(), snapshot(), result),
            json!({"op":"restart"}),
            accept(r.clone()),
            dispatch(r["request_id"].clone(), snapshot(), "ack"),
        ]))
        .unwrap();
        if result == "crash_before_ack" {
            assert_eq!(v["results"][3]["result"]["state"], "delivery_unknown");
        }
        assert!(v["results"][4]["result"]["mock_call"].is_null());
        assert!(v["results"][1]["result"]["receipt"]["effective_action"].is_null());
    }
}
#[test]
fn queue_fifo_waits_for_completion_and_late_reply_is_deduplicated() {
    let mut r = request();
    r["mode"] = json!("queue");
    let mut second = r.clone();
    second["request_id"] = json!("44444444-4444-4444-8444-444444444444");
    let mut busy = snapshot();
    busy["state"] = json!("busy");
    busy["turn_id"] = json!("previous-turn");
    let observe = |fact: &str| json!({"op":"observe","request_id":r["request_id"],"session_id":r["session_id"],"turn_id":format!("fixture-turn-{}",r["request_id"].as_str().unwrap()),"client_id":r["request_id"],"fact":fact,"item_id":"public-1","text":if fact=="reply_observed"{Some("Public reply")}else{None}});
    let v = run(scenario(vec![
        accept(r.clone()),
        accept(second.clone()),
        dispatch(r["request_id"].clone(), busy, "ack"),
        dispatch(second["request_id"].clone(), snapshot(), "ack"),
        dispatch(r["request_id"].clone(), snapshot(), "ack"),
        dispatch(second["request_id"].clone(), snapshot(), "ack"),
        observe("turn_completed"),
        dispatch(second["request_id"].clone(), snapshot(), "ack"),
        observe("reply_observed"),
        json!({"op":"restart"}),
        observe("reply_observed"),
    ]))
    .unwrap();
    for i in [2, 3, 5] {
        assert!(v["results"][i]["result"]["mock_call"].is_null());
    }
    assert_eq!(v["results"][6]["result"]["state"], "turn_completed");
    assert!(v["results"][6]["result"]["reply"].is_null());
    assert_eq!(
        v["results"][7]["result"]["mock_call"]["method"],
        "turn/start"
    );
    assert_eq!(v["results"][8], v["results"][10]);
}
#[test]
fn auth_origin_csrf_and_disabled_gate_are_independent() {
    for (field, val, code) in [
        ("authenticated", json!(false), "unauthenticated"),
        ("csrf_valid", json!(false), "csrf_rejected"),
        ("origin", json!("https://evil.example"), "origin_rejected"),
    ] {
        let mut v = scenario(vec![]);
        v[field] = val;
        assert_eq!(run(v).unwrap_err().0, code);
    }
    let mut v = scenario(vec![accept(request())]);
    v["sending_enabled"] = json!(false);
    assert_eq!(
        run(v).unwrap()["results"][0]["error"]["code"],
        "native_sending_disabled"
    );
    let mut r = request();
    r["mode"] = json!("interrupt_and_submit_pending_steers");
    assert!(serde_json::from_value::<Request>(r).is_err());
    let mut r = request();
    r["executable"] = json!("/bin/sh");
    assert!(serde_json::from_value::<Request>(r).is_err());
    let mut r: Request = serde_json::from_value(request()).unwrap();
    r.text = "x".repeat(16385);
    assert!(r.validate().is_err());
}
#[test]
fn checkins_never_force_steer_or_respond_to_agent_replies() {
    for state in [
        "idle",
        "busy",
        "paused",
        "stopped",
        "disconnected",
        "unknown",
    ] {
        let mut s = snapshot();
        s["state"] = json!(state);
        let v=run(scenario(vec![json!({"op":"check_in","snapshot":s,"last_sent":null,"user_enabled":true,"trigger":"user_timer"}),json!({"op":"check_in","snapshot":s,"last_sent":null,"user_enabled":true,"trigger":"agent_reply"}),json!({"op":"check_in","snapshot":s,"last_sent":1999999999,"user_enabled":true,"trigger":"user_timer"})])).unwrap();
        let out = &v["results"][0]["result"];
        assert_eq!(out["eligible_in_fixture"], matches!(state, "idle" | "busy"));
        if state == "busy" {
            assert_eq!(out["requested_mode"], "queue");
        }
        assert_eq!(v["results"][1]["result"]["eligible_in_fixture"], false);
        assert_eq!(v["results"][2]["result"]["eligible_in_fixture"], false);
    }
}
#[test]
fn missing_or_changed_active_turn_never_becomes_a_start() {
    for (expected, actual) in [
        (None, Some("turn-a")),
        (Some("turn-a"), None),
        (Some("turn-a"), Some("turn-b")),
    ] {
        let mut r = request();
        r["expected_turn_id"] = json!(expected);
        let mut s = snapshot();
        s["state"] = json!("busy");
        s["turn_id"] = json!(actual);
        let v = run(scenario(vec![
            accept(r.clone()),
            dispatch(r["request_id"].clone(), s, "ack"),
        ]))
        .unwrap();
        assert_eq!(
            v["results"][1]["result"]["receipt"]["reason"],
            "turn_conflict"
        );
        assert!(v["results"][1]["result"]["mock_call"].is_null());
    }
}

#[test]
fn stock_idle_submit_without_atomic_guard_reports_uncertainty() {
    let r = request();
    let mut s = snapshot();
    s.as_object_mut().unwrap().remove("atomic_idle_start");
    let v = run(scenario(vec![
        accept(r.clone()),
        dispatch(r["request_id"].clone(), s, "ack"),
    ]))
    .unwrap();
    let out = &v["results"][1]["result"];
    assert_eq!(out["receipt"]["state"], "native_accepted");
    assert!(out["receipt"]["effective_action"].is_null());
    assert_eq!(out["mock_call"]["method"], "turn/start");
}

#[test]
fn native_steer_recording_is_not_consumption_or_reply_and_survives_restart() {
    let mut r = request();
    r["expected_turn_id"] = json!("turn-a");
    let mut s = snapshot();
    s["state"] = json!("busy");
    s["turn_id"] = json!("turn-a");
    let event = json!({"method":"item/completed","params":{"threadId":r["session_id"],"turnId":"turn-a","item":{"type":"userMessage","id":"item-original","clientId":r["request_id"],"content":[{"text":"DO_NOT_PERSIST"}]}}});
    let observe = |event: Value| json!({"op":"native_observation","request_id":r["request_id"],"notification":event.to_string()});
    let mut wrong = event.clone();
    wrong["params"]["turnId"] = json!("prior-turn");
    let mut unrelated = event.clone();
    unrelated["params"]["item"]["clientId"] = json!("human");
    let mut projection = event.clone();
    projection["params"]["item"]["id"] = json!("item-projection");
    let mut private = event.clone();
    private["method"] = json!("item/reasoning/textDelta");
    let v = run(scenario(vec![accept(r.clone()), dispatch(r["request_id"].clone(), s, "timeout"),
        json!({"op":"restart"}), observe(wrong), observe(unrelated), observe(private), observe(event.clone()),
        json!({"op":"restart"}), observe(event), observe(projection),
        json!({"op":"observe","request_id":r["request_id"],"session_id":r["session_id"],"turn_id":"turn-a","client_id":r["request_id"],"fact":"turn_completed","item_id":"unused"}),
        json!({"op":"observe","request_id":r["request_id"],"session_id":r["session_id"],"turn_id":"turn-a","client_id":r["request_id"],"fact":"reply_observed","item_id":"reply-prior","text":"Unproven reply"})
    ])).unwrap();
    assert_eq!(v["results"][3]["error"]["code"], "correlation_changed");
    assert_eq!(v["results"][4]["error"]["code"], "uncorrelated");
    assert_eq!(v["results"][5]["error"]["code"], "unsupported_public_event");
    assert_eq!(v["results"][6], v["results"][8]);
    assert_eq!(v["results"][6]["result"]["state"], "input_recorded");
    assert!(
        !v["results"][6]["result"]["facts"]
            .to_string()
            .contains("consumed")
    );
    assert_eq!(v["results"][9]["error"]["code"], "correlation_changed");
    assert_eq!(v["results"][10]["result"]["state"], "turn_completed");
    assert!(v["results"][10]["result"]["reply"].is_null());
    assert_eq!(
        v["results"][11]["error"]["code"],
        "unsupported_steer_reply_correlation"
    );
    assert!(!v.to_string().contains("DO_NOT_PERSIST"));
}
