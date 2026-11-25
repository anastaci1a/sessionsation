use super::*;
#[test]
fn pane_reused_defaults_false_and_is_omitted_while_false() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/contract/fixtures.json")).unwrap();
    let mut receipt: Receipt = serde_json::from_value(fixtures["launch_receipt"].clone()).unwrap();
    assert!(!receipt.pane_reused);
    assert!(
        serde_json::to_value(&receipt)
            .unwrap()
            .get("pane_reused")
            .is_none()
    );
    receipt.pane_reused = true;
    let stored = serde_json::to_value(&receipt).unwrap();
    assert_eq!(stored["pane_reused"], true);
    assert!(
        serde_json::from_value::<Receipt>(stored)
            .unwrap()
            .pane_reused
    );
}
#[test]
fn legacy_creation_time_stays_unknown_and_admission_time_is_immutable() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/contract/fixtures.json")).unwrap();
    let mut receipt: Receipt = serde_json::from_value(fixtures["launch_receipt"].clone()).unwrap();
    assert_eq!(receipt.created_at, None);
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    save(&paths, &mut receipt).unwrap();
    assert_eq!(
        status(&paths, &receipt.request.request_id)
            .unwrap()
            .created_at,
        None
    );
    receipt.created_at = Some(123);
    receipt.phase = "running".into();
    save(&paths, &mut receipt).unwrap();
    let loaded = status(&paths, &receipt.request.request_id).unwrap();
    assert_eq!(loaded.created_at, Some(123));
    assert!(loaded.updated_at > 123);
    let mut resume_value = fixtures["resume_receipt"].clone();
    resume_value["conversation_id"] = serde_json::json!("11111111-1111-4111-8111-111111111111");
    resume_value["generation"] = serde_json::json!("legacy-generation");
    let resume: crate::resume::Receipt = serde_json::from_value(resume_value).unwrap();
    assert_eq!(resume.created_at, None);
    let deletion: crate::account_deletion::Record =
        serde_json::from_value(fixtures["account_deletion"].clone()).unwrap();
    assert_eq!(deletion.created_at, None);
}
#[test]
fn directory_preview_does_not_create_and_rejects_late_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("new");
    let d = Directory {
        path: p.clone(),
        create: vec![p.clone()],
        confirmed_existing: false,
    };
    directory_check(&d).unwrap();
    assert!(!p.exists());
    fs::create_dir(&p).unwrap();
    assert!(directory_check(&d).is_err());
    assert!(
        directory_check(&Directory {
            path: p.clone(),
            create: vec![],
            confirmed_existing: false
        })
        .is_err()
    );
    directory_check(&Directory {
        path: p,
        create: vec![],
        confirmed_existing: true,
    })
    .unwrap();
}
#[test]
fn session_limit_is_separate_from_directory_and_space_names() {
    for name in ["a".repeat(64), "界".repeat(64)] {
        validate_session_name(&name).unwrap();
    }
    for name in ["a".repeat(65), "界".repeat(65)] {
        assert!(validate_session_name(&name).is_err());
        validate_name(&name).unwrap();
    }
}
#[test]
fn hosted_launches_are_claude_only_with_a_known_permission_mode() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/contract/fixtures.json")).unwrap();
    let mut r: Request = serde_json::from_value(fixtures["launch_request"].clone()).unwrap();
    assert!(r.hosted.is_none(), "older requests stay valid");
    r.account_id = "claude:personal".into();
    r.hosted = Some(Hosted {
        permission_mode: Some("plan".into()),
    });
    shape(&r).unwrap();
    r.hosted = Some(Hosted {
        permission_mode: Some("yolo".into()),
    });
    assert!(shape(&r).is_err());
    r.hosted = Some(Hosted {
        permission_mode: None,
    });
    r.account_id = "codex:work".into();
    assert!(
        shape(&r)
            .unwrap_err()
            .to_string()
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
    let mut v = fixtures["launch_request"].clone();
    v["hosted"] = serde_json::json!({"permission_mode":"plan","extra":1});
    assert!(serde_json::from_value::<Request>(v).is_err());
}
#[test]
fn model_and_reasoning_effort_are_codex_only_validated_and_become_options() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/contract/fixtures.json")).unwrap();
    let mut r: Request = serde_json::from_value(fixtures["launch_request"].clone()).unwrap();
    assert!(
        r.model.is_none() && r.reasoning_effort.is_none(),
        "older requests stay valid"
    );
    r.account_id = "codex:work".into();
    r.model = Some("gpt-6-astra".into());
    r.reasoning_effort = Some("medium".into());
    shape(&r).unwrap();
    assert_eq!(
        model_options(r.model.as_deref(), r.reasoning_effort.as_deref()),
        [
            "-m",
            "gpt-6-astra",
            "-c",
            "model_reasoning_effort=\"medium\""
        ]
    );
    assert!(model_options(None, None).is_empty());
    for bad in ["", "-x", "a b", "a;b", "a\nb", "$(x)", &"m".repeat(101)] {
        r.model = Some(bad.into());
        assert!(
            shape(&r).unwrap_err().to_string().contains("INVALID_MODEL"),
            "{bad:?}"
        );
    }
    r.model = None;
    for bad in ["MEDIUM", "", "extreme", "medium\""] {
        r.reasoning_effort = Some(bad.into());
        assert!(
            shape(&r)
                .unwrap_err()
                .to_string()
                .contains("INVALID_REASONING_EFFORT"),
            "{bad:?}"
        );
    }
    r.reasoning_effort = Some("high".into());
    // Contract 1.49.0: Claude terminal launches take an exact id and a Claude effort.
    r.account_id = "claude:work".into();
    shape(&r).unwrap();
    r.model = Some("claude-fable-5-2".into());
    r.reasoning_effort = Some("max".into());
    shape(&r).unwrap();
    r.model = Some("fable".into());
    assert!(shape(&r).unwrap_err().to_string().contains("INVALID_MODEL"));
    r.model = Some("claude-fable-5-2".into());
    r.reasoning_effort = Some("minimal".into());
    assert!(
        shape(&r)
            .unwrap_err()
            .to_string()
            .contains("INVALID_REASONING_EFFORT")
    );
    r.reasoning_effort = Some("high".into());
    r.hosted = Some(Hosted {
        permission_mode: None,
    });
    assert!(
        shape(&r)
            .unwrap_err()
            .to_string()
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
    r.hosted = None;
    r.account_id = "gemini:g".into();
    assert!(
        shape(&r)
            .unwrap_err()
            .to_string()
            .contains("UNSUPPORTED_PROVIDER_OPERATION")
    );
    // A request with a model still reads as a 1.48-shaped request (same fields).
    r.account_id = "claude:work".into();
    let bytes = serde_json::to_value(&r).unwrap();
    assert_eq!(serde_json::from_value::<Request>(bytes).unwrap(), r);
    let mut v = fixtures["launch_request"].clone();
    v["model"] = serde_json::json!(5);
    assert!(serde_json::from_value::<Request>(v).is_err());
}
#[test]
fn request_id_and_name_cannot_be_paths() {
    assert!(validate_id("../x").is_err());
    for n in ["../x", ".", "..", " x", "x/y"] {
        assert!(validate_name(n).is_err());
    }
}
fn fixture(paths: &Paths, phase: &str) -> String {
    fixture_named(paths, phase, "stuck-name")
}
fn fixture_named(paths: &Paths, phase: &str, name: &str) -> String {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/contract/fixtures.json")).unwrap();
    let mut r: Receipt = serde_json::from_value(fixtures["launch_receipt"].clone()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    r.request.request_id = id.clone();
    r.request.name = name.into();
    r.phase = phase.into();
    r.pane_id = None;
    save(paths, &mut r).unwrap();
    catalog::reserve_name(paths, None, name, &id).unwrap();
    id
}
#[test]
fn ack_timeout_while_still_dispatching_fails_and_releases_the_name() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let id = fixture(&paths, "dispatching");
    let e = expire_ack(&paths, &id).unwrap_err().to_string();
    assert!(e.starts_with("LAUNCH_RUN_NEVER_STARTED"));
    let r = status(&paths, &id).unwrap();
    assert_eq!(r.phase, "failed");
    assert!(r.detail.unwrap().contains("LAUNCH_RUN_NEVER_STARTED"));
    catalog::reserve_name(&paths, None, "stuck-name", "other").unwrap();
}
#[test]
fn ack_timeout_after_launch_run_claimed_stays_uncertain() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let id = fixture(&paths, "starting_provider");
    assert_eq!(
        expire_ack(&paths, &id).unwrap_err().to_string(),
        "LAUNCH_ACK_UNCERTAIN"
    );
    assert_eq!(status(&paths, &id).unwrap().phase, "starting_provider");
    assert!(catalog::reserve_name(&paths, None, "stuck-name", "other").is_err());
}
#[test]
fn a_pre_start_refusal_is_failed_but_a_possible_start_stays_uncertain() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let error = anyhow::anyhow!("ACCOUNT_BUSY: an account operation held the lock for 120s");
    assert_eq!(
        failure_line(&error),
        "sessionsation launch failed: ACCOUNT_BUSY: an account operation held the lock for 120s"
    );
    let id = fixture_named(&paths, "starting_provider", "pre-start");
    note_account_wait(&paths, &id);
    assert_eq!(
        status(&paths, &id).unwrap().detail.as_deref(),
        Some(ACCOUNT_WAIT_DETAIL)
    );
    settle_run_failure(&paths, &id, &error, false).unwrap();
    let r = status(&paths, &id).unwrap();
    assert_eq!(r.phase, "failed");
    assert!(r.detail.unwrap().starts_with("ACCOUNT_BUSY"));
    catalog::reserve_name(&paths, None, "pre-start", "other").unwrap();
    let id = fixture_named(&paths, "starting_provider", "maybe-started");
    settle_run_failure(&paths, &id, &anyhow::anyhow!("lost reply"), true).unwrap();
    assert_eq!(status(&paths, &id).unwrap().phase, "uncertain");
    assert!(catalog::reserve_name(&paths, None, "maybe-started", "other").is_err());
}
#[tokio::test]
async fn run_refuses_after_the_ack_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let id = fixture(&paths, "dispatching");
    let _ = expire_ack(&paths, &id);
    let e = run(&paths, &id).await.unwrap_err().to_string();
    assert_eq!(e, "LAUNCH_ALREADY_DISPATCHED");
    assert_eq!(status(&paths, &id).unwrap().phase, "failed");
}
#[tokio::test]
async fn abandon_uncertain_launch_releases_name_and_refuses_when_a_session_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let e = |r: Result<Value>| r.unwrap_err().to_string();
    let blocked = fixture_named(&paths, "uncertain", "blocked-name");
    catalog::complete_reserved(
        &paths,
        &blocked,
        None,
        "blocked-name",
        ProviderId::Codex,
        "fixture",
        "conv",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        e(abandon(&paths, &blocked, &blocked).await),
        "SESSION_EXISTS"
    );
    let fresh = fixture_named(&paths, "dispatching", "fresh-name");
    assert_eq!(
        e(abandon(&paths, &fresh, &fresh).await),
        "OPERATION_NOT_STUCK"
    );
    let id = fixture(&paths, "uncertain");
    assert_eq!(e(abandon(&paths, &id, "x").await), "CONFIRMATION_MISMATCH");
    let done = abandon(&paths, &id, &id).await.unwrap();
    assert_eq!(done["phase"], "abandoned");
    assert_eq!(done["abandoned_from"], "uncertain");
    assert_eq!(
        e(abandon(&paths, &id, &id).await),
        "OPERATION_ALREADY_TERMINAL"
    );
    catalog::reserve_name(&paths, None, "stuck-name", "other").unwrap();
}
/// Kept from S2 (adapted: hosted Claude is restored in 1.70.0): a receipt request with
/// `hosted` reads and keeps the field; the shape check takes it for Claude only.
#[test]
fn a_legacy_receipt_request_with_hosted_still_reads_and_a_new_one_is_accepted() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../docs/contract/fixtures.json")).unwrap();
    let mut v = fixtures["launch_request"].clone();
    v["hosted"] = json!({"permission_mode": "default"});
    let r: Request = serde_json::from_value(v.clone()).unwrap();
    assert!(r.hosted.is_some());
    assert!(serde_json::to_string(&r).unwrap().contains("hosted"));
    if !r.account_id.starts_with("claude:") {
        let e = shape(&r).unwrap_err().to_string();
        assert!(e.contains("hosted sessions are Claude-only"), "{e}");
    }
    v["account_id"] = json!("claude:work");
    v["model"] = serde_json::Value::Null;
    v["reasoning_effort"] = serde_json::Value::Null;
    shape(&serde_json::from_value(v).unwrap()).unwrap();
}
