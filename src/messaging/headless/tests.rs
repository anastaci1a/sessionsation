use super::*;
#[test]
fn from_component_is_taken_out_and_validated() {
    let mut v = json!({"mode":"submit","from_component":"resource-guard"});
    assert_eq!(
        take_component(&mut v).unwrap().as_deref(),
        Some("resource-guard")
    );
    assert_eq!(v, json!({"mode":"submit"}), "typed requests never see it");
    let mut v = json!({"mode":"submit","from_component":null});
    assert_eq!(take_component(&mut v).unwrap(), None);
    assert_eq!(take_component(&mut json!({"mode":"submit"})).unwrap(), None);
    for bad in [
        json!("bad name"),
        json!(""),
        json!("-x"),
        json!("a".repeat(41)),
        json!(7),
    ] {
        let e = take_component(&mut json!({"from_component":bad}))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("INVALID_ARGUMENT: from_component"), "{e}");
    }
}
#[test]
fn print_args_never_carry_the_message_and_select_the_session_flag() {
    let id = "11111111-1111-4111-8111-111111111111";
    assert_eq!(
        print_args(ProviderId::Claude, Mode::Resume, id),
        ["-p", "--resume", id, "--output-format", "json"]
    );
    assert_eq!(
        print_args(ProviderId::Claude, Mode::New, id)[1],
        "--session-id"
    );
    let g = print_args(ProviderId::Gemini, Mode::Resume, id);
    assert_eq!(
        g,
        [
            "-p",
            "",
            "-o",
            "json",
            "--approval-mode",
            "default",
            "-r",
            id
        ]
    );
    assert!(!g.iter().any(|a| a == "yolo" || a == "--yolo" || a == "-y"));
    assert_eq!(
        print_args(ProviderId::Gemini, Mode::New, id)[6],
        "--session-id"
    );
}
#[test]
fn result_parsing_accepts_object_or_event_array_only() {
    let c = ProviderId::Claude;
    let o = parse(
        c,
        br#"{"type":"result","result":"hi","session_id":"s","is_error":false}"#,
        Some(0),
    )
    .unwrap();
    assert_eq!(o["result"], "hi");
    let a = parse(
        c,
        br#"[{"type":"system"},{"type":"result","result":"x","session_id":"s"}]"#,
        Some(0),
    )
    .unwrap();
    assert_eq!(a["result"], "x");
    assert!(parse(c, b"not json", Some(0)).is_none());
    assert!(parse(c, br#"{"type":"system"}"#, Some(0)).is_none());
    assert!(parse(c, b"[]", Some(0)).is_none());
    let g = ProviderId::Gemini;
    let ok = parse(
        g,
        br#"{"session_id":"s","response":"hi","stats":{"models":{}}}"#,
        Some(0),
    )
    .unwrap();
    assert_eq!(ok["result"], "hi");
    assert_eq!(ok["is_error"], false);
    let failed = parse(
        g,
        br#"{"session_id":"s","error":{"type":"FatalError","message":"x"}}"#,
        Some(1),
    )
    .unwrap();
    assert_eq!(failed["is_error"], true);
    assert_eq!(failed["subtype"], "FatalError");
    let exit = parse(g, br#"{"session_id":"s","response":"cut"}"#, Some(53)).unwrap();
    assert_eq!(exit["is_error"], true);
    assert_eq!(exit["subtype"], "exit_53");
    assert!(parse(g, b"[]", Some(0)).is_none());
    assert!(parse(g, br#"{"other":1}"#, Some(0)).is_none());
    assert!(parse(g, b"plain text", Some(0)).is_none());
}
#[test]
fn stale_worker_projection_is_never_success_and_never_replays() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("root"))).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut r = Record {
        schema_version: 1,
        provider: ProviderId::Claude,
        request: Request {
            api_version: 1,
            message_id: id.clone(),
            mode: Mode::Resume,
            session_id: Some(uuid::Uuid::new_v4().to_string()),
            expected_generation: Some("a".repeat(64)),
            account_id: None,
            directory: None,
            name: None,
            text: "private".into(),
            expires_at: 1,
            timeout_seconds: None,
            recovery: None,
            steering: None,
        },
        session_id: "s".into(),
        conversation_id: "c".into(),
        account: "a".into(),
        directory: "/".into(),
        accepted_generation: "g".into(),
        timeout_seconds: 1,
        phase: "executing".into(),
        state: "execution_started".into(),
        reason: None,
        reply: None,
        reported_session_id: None,
        created_at: 0,
        updated_at: 0,
        started_at: Some(0),
        finished_at: None,
    };
    save(&paths, &mut r).unwrap();
    let p = project(&paths, read(&paths, &id).unwrap()).unwrap();
    assert_eq!(p.state, "delivery_unknown");
    assert_eq!(
        read(&paths, &id).unwrap().phase,
        "executing",
        "projection only"
    );
    let v = receipt_value(&p);
    assert!(!v.to_string().contains("private"));
    assert_eq!(v["facts"]["reply_observed"], false);
    assert!(
        active_for_account(&paths, ProviderId::Claude, "a")
            .unwrap()
            .is_empty()
    );
    r.phase = "admitted".into();
    r.state = "pending_local".into();
    r.updated_at = 0;
    atomic_write(
        &file(&paths, &id).unwrap(),
        &serde_json::to_vec(&r).unwrap(),
    )
    .unwrap();
    assert_eq!(
        project(&paths, read(&paths, &id).unwrap()).unwrap().state,
        "rejected"
    );
}
mod gemini_auth_tests;
mod gemini_sign_in_refusal_tests;
