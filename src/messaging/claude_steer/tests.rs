use super::*;
#[test]
fn queue_reason_names_the_real_peer_blocker() {
    // Terminal available, live steering on, socket unlinked.
    let unlinked: Result<(), String> = Err(claude::peer::SOCKET_UNLINKED.into());
    let r = queue_blocked_reason(true, &unlinked).unwrap();
    assert!(r.starts_with("PEER_SOCKET_UNLINKED"), "{r}");
    assert!(!r.contains("enable live steering"), "{r}");
    let off: Result<(), String> = Err(claude::peer::DISABLED.into());
    let r = queue_blocked_reason(true, &off).unwrap();
    assert!(
        r.starts_with("TERMINAL_QUEUE_UNSUPPORTED") && r.contains("enable live steering"),
        "{r}"
    );
    let r = queue_blocked_reason(false, &off).unwrap();
    assert!(r.starts_with("LIVE_STEERING_DISABLED"), "{r}");
    assert_eq!(queue_blocked_reason(true, &Ok(())), None);
}
#[test]
fn peer_failures_keep_their_own_code_and_ok() {
    let e = anyhow::anyhow!(claude::peer::SOCKET_UNLINKED);
    assert!(peer_endpoint_reason(&e).starts_with("PEER_SOCKET_UNLINKED"));
    let e = anyhow::anyhow!("PERMISSION_DENIED");
    assert_eq!(peer_endpoint_reason(&e), "PERMISSION_DENIED");
    let e = anyhow::anyhow!("socket is stale");
    assert_eq!(
        peer_endpoint_reason(&e),
        "PEER_ENDPOINT_UNAVAILABLE: socket is stale"
    );
    assert_eq!(
        terminal_unavailable("TERMINAL_INPUT_DISABLED").to_string(),
        terminal::DISABLED
    );
}
#[test]
fn transcript_scan_needs_the_anchor_and_stops_at_the_next_input() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("t.jsonl");
    let id = "22222222-2222-4222-8222-222222222222";
    let lines = [
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"before"}]}}),
        json!({"type":"user","isMeta":true,"origin":{"msg_id":id},"message":{"content":"peer text"}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"one"},{"type":"tool_use","name":"Bash"}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}),
        json!({"type":"user","message":{"content":"next input"}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"after"}]}}),
    ];
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(&p, &text).unwrap();
    let s = scan(&p, 0, id).unwrap();
    assert!(s.anchor_found && s.turn_ended);
    assert_eq!(s.reply, ["one", "two"]);
    // Anchor absent: nothing is attributed.
    let s = scan(&p, 0, "33333333-3333-4333-8333-333333333333").unwrap();
    assert_eq!(s, Scan::default());
    // An unfinished turn is not ended; an assistant record naming the id is no anchor.
    fs::write(
        &p,
        format!(
            "{}\n{}\n{}\n",
            json!({"type":"assistant","message":{"content":[{"type":"text","text":id}]}}),
            lines[1],
            lines[2]
        ),
    )
    .unwrap();
    let s = scan(&p, 0, id).unwrap();
    assert!(s.anchor_found && !s.turn_ended);
    assert_eq!(s.reply, ["one"]);
}
#[test]
fn requests_accept_only_claude_steering_options() {
    let base = json!({"api_version":1,"message_id":uuid::Uuid::new_v4().to_string(),"mode":"steer",
        "session_id":uuid::Uuid::new_v4().to_string(),"expected_generation":"a".repeat(64),"text":"x","expires_at":1});
    let ok: Request = serde_json::from_value(base.clone()).unwrap();
    assert!(shape(&ok).is_ok());
    let mut v = base.clone();
    v["steering"] = json!({"version":1,"interrupt":true});
    assert!(shape(&serde_json::from_value(v.clone()).unwrap()).is_ok());
    v["mode"] = json!("queue");
    assert!(shape(&serde_json::from_value(v).unwrap()).is_err());
    let mut codex = base.clone();
    codex["steering"] = json!({"version":1,"expected_turn_id":"t"});
    assert!(serde_json::from_value::<Request>(codex).is_err());
    assert_eq!(priority("steer"), "next");
    assert_eq!(priority("submit"), "next");
    assert_eq!(priority("queue"), "later");
}

fn peer_record(id: &str, allowed: bool, written: bool) -> Record {
    serde_json::from_value(json!({"schema_version":1,"kind":"peer",
        "request":{"api_version":1,"message_id":id,"mode":"steer","session_id":"s","expected_generation":"g",
            "text":"hello","expires_at":i64::MAX,"origin":"agent","delivery":"native"},
        "session_id":"s","conversation_id":"c","account":"a","accepted_generation":"g","priority":"next",
        "host_epoch":null,"phase":"dispatching","state":"pending_local","reason":null,
        "facts":{"written":written,"lifecycle":[],"input_recorded":false,"folded":false,"shared_with_turn":false,
            "interrupt_requested":false,"peer_status":[],"idle_notice":null},
        "reply_text":"","reply_truncated":false,"result":null,
        "peer":{"pid":1,"process_start":1,"socket":"/x/1.sock","transcript":null,"anchor_offset":null,
            "notify_msg_id":"n","anchor_found":false,"turn_ended":false},
        "terminal":terminal::Terminal { target: "herdr/x".into(), pid: 1, delivered_text: "hello".into(), ..Default::default() },
        "fallback_allowed":allowed,"observation_error":null,"created_at":1,"updated_at":1,
        "dispatched_at":1,"started_at":null,"finished_at":null,"evidence_at":null}))
    .unwrap()
}

#[test]
fn an_unsent_native_request_becomes_terminal_input_exactly_once() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("root"))).unwrap();
    private_dir(&dir(&paths)).unwrap();
    let (a, b, c) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    for (id, allowed, written) in [(&a, true, false), (&b, true, true), (&c, false, false)] {
        save(&paths, &mut peer_record(id, allowed, written)).unwrap();
    }
    // Connection never opened: converted, not yet complete, native evidence gone.
    assert!(fall_back_to_terminal(&paths, &a, "PEER_SOCKET_UNREACHABLE", "delivery").unwrap());
    let r = read(&paths, &a).unwrap();
    assert_eq!(
        (r.kind.as_str(), r.phase.as_str()),
        ("terminal", "admitted")
    );
    assert!(r.peer.is_none());
    assert_eq!(r.fallback.unwrap()["reason"], "PEER_SOCKET_UNREACHABLE");
    // Converting again is a no-op (the record is no longer a peer record).
    assert!(!fall_back_to_terminal(&paths, &a, "x", "delivery").unwrap());
    // A byte was written, or no fallback was prepared: never converted.
    assert!(!fall_back_to_terminal(&paths, &b, "PEER_WRITE_FAILED", "delivery").unwrap());
    assert!(!fall_back_to_terminal(&paths, &c, "PEER_SOCKET_UNREACHABLE", "delivery").unwrap());
    for id in [&b, &c] {
        let r = read(&paths, id).unwrap();
        assert_eq!(r.kind, "peer");
        assert!(r.fallback.is_none());
    }
}
