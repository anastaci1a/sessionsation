use super::*;

#[test]
fn brief_rules_follow_the_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    assert!(check_brief(b"", "first_turn", &paths).is_err());
    assert!(check_brief(b"a\0b", "first_turn", &paths).is_err());
    assert!(check_brief(&[0xff, 0xfe], "first_turn", &paths).is_err());
    check_brief(b"do the thing", "first_turn", &paths).unwrap();
    assert!(
        check_brief(&vec![b'a'; CODEX_MAX + 1], "first_turn", &paths)
            .unwrap_err()
            .to_string()
            .starts_with("INPUT_TOO_LARGE")
    );
    assert!(check_brief(b"/model x", "initial_prompt", &paths).is_err());
    check_brief(b"plain brief", "initial_prompt", &paths).unwrap();
    assert!(
        check_brief(&vec![b'a'; CLAUDE_HELD_MAX + 1], "held", &paths)
            .unwrap_err()
            .to_string()
            .starts_with("TERMINAL_TEXT_TOO_LONG")
    );
    assert!(check_brief(b"a\tb", "held", &paths).is_err());
}

fn args(dir: &Path) -> Args {
    Args {
        space: "w1".into(),
        create_space: false,
        name: "n".into(),
        account: "claude:a".into(),
        model: "claude-fable-5-2".into(),
        effort: Some("high".into()),
        brief_file: dir.join("brief"),
        when_ready: false,
        initial_prompt: false,
        directory: Some(dir.to_path_buf()),
        create_directory: false,
        permissions: None,
        herdr_session: Some("default".into()),
        herdr_socket: Some(dir.join("herdr.sock")),
        request_id: None,
        deadline: None,
        wait: None,
        dry_run: false,
    }
}

#[test]
fn request_carries_exact_model_and_a_confirmed_directory() {
    let dir = tempfile::tempdir().unwrap();
    let a = args(dir.path());
    let id = "33333333-3333-4333-8333-333333333333";
    let r = request(&a, id).unwrap();
    assert_eq!(r.model.as_deref(), Some("claude-fable-5-2"));
    assert_eq!(r.reasoning_effort.as_deref(), Some("high"));
    assert!(r.directory.confirmed_existing && r.directory.create.is_empty());
    assert_eq!(r.space, launch::Space::Existing { id: "w1".into() });
    let mut missing = args(dir.path());
    missing.directory = Some(dir.path().join("x/y"));
    assert!(
        request(&missing, id)
            .unwrap_err()
            .to_string()
            .starts_with("DIRECTORY_CONFIRMATION_REQUIRED")
    );
    missing.create_directory = true;
    let r = request(&missing, id).unwrap();
    assert_eq!(
        r.directory.create,
        [dir.path().join("x"), dir.path().join("x/y")]
    );
    assert!(!r.directory.confirmed_existing);
}

#[tokio::test]
async fn admission_refuses_aliases_and_bad_combinations_before_any_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    fs::write(dir.path().join("brief"), "brief").unwrap();
    let mut a = args(dir.path());
    a.model = "fable".into();
    let e = launch(&paths, a).await.unwrap_err().to_string();
    assert!(e.starts_with("INVALID_MODEL"), "{e}");
    let mut a = args(dir.path());
    a.when_ready = true;
    a.initial_prompt = true;
    assert!(
        launch(&paths, a)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_ARGUMENT")
    );
    let mut a = args(dir.path());
    a.account = "gemini:g".into();
    assert!(
        launch(&paths, a)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("UNSUPPORTED_PROVIDER_OPERATION")
    );
    let mut a = args(dir.path());
    a.deadline = Some(5);
    assert!(
        launch(&paths, a)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_ARGUMENT")
    );
    let mut a = args(dir.path());
    a.request_id = Some("not-a-uuid".into());
    assert!(
        launch(&paths, a)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("INVALID_REQUEST_ID")
    );
    assert!(
        !dir.path()
            .join("root")
            .join(crate::layout::SESSION_LAUNCHES)
            .exists()
    );
}

fn sending(paths: &Paths, deadline_in: i64) -> (String, String) {
    let id = uuid::Uuid::new_v4().to_string();
    let mid = uuid::Uuid::new_v4().to_string();
    let dir = tempfile::tempdir().unwrap();
    let now = crate::daemon::now();
    let r = Record {
        schema_version: 1,
        id: id.clone(),
        request: request(&args(dir.path()), &id).unwrap(),
        provider: "codex".into(),
        brief: Brief {
            delivery: "first_turn".into(),
            requested_delivery: None,
            state: "sending".into(),
            sha256: "x".into(),
            bytes: 5,
            attempts: 1,
            message_id: Some(mid.clone()),
            message_ids: vec![mid.clone()],
            receipt_state: None,
            reason: None,
            next_attempt_at: 0,
        },
        model_requested: "gpt-6".into(),
        effort_requested: None,
        observed: None,
        model_confirmed: true,
        effort_confirmed: false,
        phase: "delivering_brief".into(),
        reason: None,
        detail: None,
        session_id: Some("s".into()),
        conversation_id: Some("c".into()),
        created_at: now,
        updated_at: now,
        deadline_at: now + deadline_in,
    };
    save(paths, &r).unwrap();
    fs::write(brief_file(paths, &id).unwrap(), "brief").unwrap();
    (id, mid)
}

#[test]
fn a_brief_is_resent_only_when_provably_nothing_was_written() {
    for s in ["rejected", "failed", "expired", "cancelled"] {
        assert!(resend_allowed(
            s,
            Some(&json!({"state":s,"facts":{"written":false}}))
        ));
        assert!(!resend_allowed(
            s,
            Some(&json!({"state":s,"facts":{"written":true}}))
        ));
        assert!(!resend_allowed(
            s,
            Some(&json!({"state":s,"facts":{"accepted_native":true}}))
        ));
    }
    assert!(resend_allowed("not_found", None));
    for s in [
        "pending_local",
        "dispatching",
        "delivery_unknown",
        "unknown",
    ] {
        assert!(!resend_allowed(s, Some(&json!({"state":s}))));
    }
}

#[tokio::test]
async fn an_unreadable_receipt_is_read_again_never_resent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let (id, mid) = sending(&paths, 300);
    // The ledger cannot be opened: the read fails for a reason other than "not found".
    fs::create_dir_all(&paths.root).unwrap();
    fs::write(paths.root.join("messaging"), "not a directory").unwrap();
    advance(&paths, &id).await;
    let r = read(&paths, &id).unwrap();
    assert_eq!(r.brief.message_id.as_deref(), Some(mid.as_str()));
    assert_eq!((r.brief.state.as_str(), r.brief.attempts), ("sending", 1));
    assert_eq!(r.brief.message_ids.len(), 1, "no second send");
    assert_eq!(r.phase, "delivering_brief");
}

#[tokio::test]
async fn a_receipt_that_does_not_exist_allows_one_new_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let (id, _) = sending(&paths, 300);
    advance(&paths, &id).await;
    let r = read(&paths, &id).unwrap();
    assert_eq!(r.brief.message_id, None);
    assert_eq!(r.brief.state, "held");
    assert_eq!(r.brief.receipt_state.as_deref(), Some("not_found"));
    assert!(r.brief.next_attempt_at > 0);
}

#[tokio::test]
async fn a_deadline_during_a_send_is_delivery_unknown_not_unsent() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let (id, _) = sending(&paths, -1);
    fs::create_dir_all(&paths.root).unwrap();
    fs::write(paths.root.join("messaging"), "not a directory").unwrap();
    advance(&paths, &id).await;
    let r = read(&paths, &id).unwrap();
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("ready_timeout"))
    );
    assert_eq!(r.brief.state, "delivery_unknown");
    assert_eq!(r.brief.reason.as_deref(), Some("brief_delivery_unknown"));
    // Provably unsent at the deadline: an ordinary timeout with the brief not sent.
    let dir2 = tempfile::tempdir().unwrap();
    let paths2 = Paths::new(Some(dir2.path().join("root"))).unwrap();
    let (id2, _) = sending(&paths2, -1);
    advance(&paths2, &id2).await;
    let r2 = read(&paths2, &id2).unwrap();
    assert_eq!(r2.reason.as_deref(), Some("ready_timeout"));
    assert_eq!(r2.brief.state, "not_sent");
}

#[tokio::test]
async fn an_admitted_record_waits_for_the_launch_ack_window() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(dir.path().join("root"))).unwrap();
    let (id, _) = sending(&paths, 300);
    let mut r = read(&paths, &id).unwrap();
    r.phase = "admitted".into();
    r.created_at = crate::daemon::now() - 60;
    save(&paths, &r).unwrap();
    advance(&paths, &id).await;
    assert_eq!(read(&paths, &id).unwrap().phase, "admitted");
    r.created_at = crate::daemon::now() - ADMIT_GRACE_SECONDS - 1;
    save(&paths, &r).unwrap();
    advance(&paths, &id).await;
    let r = read(&paths, &id).unwrap();
    assert_eq!(
        (r.phase.as_str(), r.reason.as_deref()),
        ("failed", Some("launch_failed"))
    );
}
