#[test]
fn a_move_destination_falls_back_to_the_visible_name_and_reuses_an_existing_name() {
    let available =
        json!({"spaces":[{"id":"wR","name":"localica"},{"id":"w21","name":"sessionstuffs"}]});
    let mut by_name = Target {
        space: Some("localica".into()),
        new_space: None,
        ..serde_json::from_value::<Target>(
            json!({"session":"s","expected_generation":"g","space":"x","request_id":"r"}),
        )
        .unwrap()
    };
    normalize_target(&mut by_name, &available);
    assert_eq!(by_name.space.as_deref(), Some("wR"));
    let mut reuse = Target {
        space: None,
        new_space: Some("sessionstuffs".into()),
        ..serde_json::from_value::<Target>(
            json!({"session":"s","expected_generation":"g","space":"x","request_id":"r"}),
        )
        .unwrap()
    };
    normalize_target(&mut reuse, &available);
    assert_eq!(
        (reuse.space.as_deref(), reuse.new_space.as_deref()),
        (Some("w21"), None)
    );
    check_destination(&reuse, &available).unwrap();
}
use super::*;
fn fixture(paths: &Paths) -> Operation {
    let thread = uuid::Uuid::new_v4().to_string();
    let source:Instance=serde_json::from_value(json!({"pane":"herdr/fixture/w1:p1","tmux_session":"fixture","pid":123,"start_time":456,"cwd":"/tmp/example","thread_id":thread,"rollout":"/tmp/example/history.jsonl","home":"/tmp/example/home","binary":"/bin/false","resume_options":[]})).unwrap();
    let session = crate::catalog::record_launch(
        paths,
        "example",
        &thread,
        "fixture",
        Some(Attachment {
            host: "herdr".into(),
            target: source.pane.clone(),
            pid: source.pid,
            process_start: source.start_time,
        }),
    )
    .unwrap();
    Operation {
        request: Target {
            session: session.id.clone(),
            expected_generation: session.generation(),
            space: Some("w2".into()),
            new_space: None,
            create_space: None,
            request_id: uuid::Uuid::new_v4().to_string(),
        },
        session,
        source,
        socket: paths.root.join("owned.sock"),
        terminal_id: "terminal-fixture".into(),
        epoch: None,
        previous_space: "w1".into(),
        known_spaces: vec![],
        created_space: None,
        phase: String::new(),
        detail: String::new(),
        pane: None,
        created_at: 1,
        updated_at: 1,
    }
}
#[test]
fn new_space_is_exclusive_fresh_and_legacy_journals_still_load() {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        target: Target,
    }
    let base = [
        "x",
        "--session",
        "s",
        "--expected-generation",
        "g",
        "--request-id",
        "r",
    ];
    let parse = |extra: &[&str]| Cli::try_parse_from(base.iter().chain(extra));
    assert!(parse(&[]).is_err());
    assert!(parse(&["--space", "w1", "--new-space", "N"]).is_err());
    assert_eq!(
        parse(&["--new-space", "N"])
            .unwrap()
            .target
            .new_space
            .as_deref(),
        Some("N")
    );
    assert_eq!(
        parse(&["--space", "w1"]).unwrap().target.space.as_deref(),
        Some("w1")
    );
    let old: Target = serde_json::from_value(
        json!({"session":"s","expected_generation":"g","space":"w2","request_id":"r"}),
    )
    .unwrap();
    assert_eq!(
        (old.space.as_deref(), old.new_space.as_deref()),
        (Some("w2"), None)
    );
    let available = json!({"spaces":[{"id":"w1","name":"Main"}]});
    assert!(
        fresh_label(&available, "Main")
            .unwrap_err()
            .to_string()
            .contains("WORKSPACE_NAME_EXISTS")
    );
    fresh_label(&available, "Other").unwrap();
    let mut request = old;
    request.space = None;
    request.new_space = Some("../bad".into());
    assert!(check_destination(&request, &available).is_err());
}
#[test]
fn move_restores_session_label_on_the_new_tab() {
    assert_eq!(
        relabel_args(Some("w2:t2"), "My session").unwrap(),
        ["tab", "rename", "w2:t2", "My session"]
    );
    assert!(relabel_args(None, "My session").is_none());
    assert!(relabel_args(Some("w2:t2"), "").is_none());
}
#[test]
fn pre_dispatch_crash_releases_only_its_reservation_without_host_rpc() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut op = fixture(&paths);
    save(&paths, &mut op, "admitted", "prepared").unwrap();
    assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_err());
    assert_eq!(
        recover(&paths, &op.request.request_id).unwrap()["phase"],
        "failed"
    );
    assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_ok());
    assert_eq!(
        crate::catalog::list(&paths).unwrap()[0].generation(),
        op.session.generation()
    );
}
#[test]
fn worker_refuses_changed_target_before_host_dispatch_and_releases_reservation() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut op = fixture(&paths);
    save(&paths, &mut op, "admitted", "prepared").unwrap();
    crate::catalog::rename(&paths, &op.session.id, &op.session.generation(), "Changed").unwrap();
    worker(&paths, &op.request.request_id).unwrap();
    let failed = read(&paths, &op.request.request_id).unwrap();
    assert_eq!(failed.phase, "failed");
    assert!(failed.detail.contains("STALE_TARGET"), "{}", failed.detail);
    assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_ok());
}

#[test]
fn duplicate_workers_never_replay_dispatch_or_terminal_records() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut op = fixture(&paths);
    for phase in [
        "dispatching",
        "reconciling",
        "uncertain",
        "complete",
        "failed",
    ] {
        save(&paths, &mut op, phase, "must remain unchanged").unwrap();
        let original = fs::read(file(&paths, &op.request.request_id).unwrap()).unwrap();
        worker(&paths, &op.request.request_id).unwrap();
        assert_eq!(
            fs::read(file(&paths, &op.request.request_id).unwrap()).unwrap(),
            original
        );
    }
}

#[tokio::test]
async fn original_request_is_observation_only_and_changed_payload_never_replays() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut op = fixture(&paths);
    save(&paths, &mut op, "dispatching", "unconfirmed").unwrap();
    assert_eq!(
        start(&paths, op.request.clone()).await.unwrap()["phase"],
        "dispatching"
    );
    let mut changed = op.request.clone();
    changed.space = Some("w3".into());
    assert!(
        start(&paths, changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("REQUEST_ID_CONFLICT")
    );
    assert_eq!(
        read(&paths, &op.request.request_id).unwrap().phase,
        "dispatching"
    );
    assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_err());
}
#[test]
fn saved_location_follows_verified_moves_without_rewriting_launch_receipts() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let mut op = fixture(&paths);
    let id = op.session.id.clone();
    let original = json!({"session":"fixture","socket":op.socket,"workspace_id":"w1","pane_id":"w1:p1","tab_id":"w1:t1"});
    op.pane = Some(
        json!({"pane_id":"w2:p2","workspace_id":"w2","tab_id":"w2:t2","terminal_id":op.terminal_id}),
    );
    save(&paths, &mut op, "complete", "done").unwrap();
    let mut next = fixture(&paths);
    next.session = op.session.clone();
    next.session.revision += 1;
    next.request.session = id.clone();
    next.request.expected_generation = next.session.generation();
    next.source = op.source.clone();
    next.source.pane = "herdr/fixture/w2:p2".into();
    next.request.space = Some("w3".into());
    next.pane = Some(
        json!({"pane_id":"w3:p3","workspace_id":"w3","tab_id":"w3:t3","terminal_id":op.terminal_id}),
    );
    save(&paths, &mut next, "uncertain", "not confirmed").unwrap();
    assert_eq!(
        follow_location(&paths, &id, original.clone(), None).unwrap()["workspace_id"],
        "w2"
    );
    save(&paths, &mut next, "complete", "done").unwrap();
    assert_eq!(
        follow_location(&paths, &id, original.clone(), None).unwrap()["workspace_id"],
        "w3"
    );
    let mut back = fixture(&paths);
    back.session = next.session.clone();
    back.session.revision += 1;
    back.request.session = id.clone();
    back.request.expected_generation = back.session.generation();
    back.source = next.source.clone();
    back.source.pane = "herdr/fixture/w3:p3".into();
    back.request.space = Some("w1".into());
    back.pane = Some(
        json!({"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","terminal_id":op.terminal_id}),
    );
    save(&paths, &mut back, "complete", "returned").unwrap();
    assert_eq!(
        follow_location(&paths, &id, original.clone(), None).unwrap()["workspace_id"],
        "w1"
    );
    assert_eq!(last_location(&paths, &id).unwrap()["workspace_id"], "w1");
    assert_eq!(
        follow_location(&paths, &id, original.clone(), Some("newer-resume-request")).unwrap(),
        original,
        "reused pane IDs in a later launch must not follow old moves"
    );
    assert_eq!(original["workspace_id"], "w1");
    let unrelated = json!({"session":"other","socket":"/other.sock","pane_id":"w1:p1"});
    assert_eq!(
        follow_location(&paths, &id, unrelated.clone(), None).unwrap(),
        unrelated
    );
}

/// Space-move journals written by the 1.72.0 base (shrink-C3) load and re-save byte for byte.
#[test]
fn base_space_move_journals_resave_byte_for_byte() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/handoff-jobs");
    for phase in ["admitted", "uncertain", "complete"] {
        let bytes = fs::read(format!("{fixtures}/space-move-{phase}.json")).unwrap();
        let id = serde_json::from_slice::<Value>(&bytes).unwrap()["request"]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
        atomic_write(&file(&paths, &id).unwrap(), &bytes).unwrap();
        let mut op = read(&paths, &id).unwrap();
        assert_eq!(op.phase, phase);
        assert_eq!(serde_json::to_vec(&op).unwrap(), bytes, "{phase}");
        let detail = op.detail.clone();
        save(&paths, &mut op, phase, &detail).unwrap();
        let mut saved: Value =
            serde_json::from_slice(&fs::read(file(&paths, &id).unwrap()).unwrap()).unwrap();
        let mut expected: Value = serde_json::from_slice(&bytes).unwrap();
        // Only the stamp moves.
        saved["updated_at"] = Value::Null;
        expected["updated_at"] = Value::Null;
        assert_eq!(saved, expected);
        assert_eq!(operations(&paths).unwrap().len(), 1);
        fs::remove_file(file(&paths, &id).unwrap()).unwrap();
    }
}
