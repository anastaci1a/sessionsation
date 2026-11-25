use super::*;

fn dead() -> Info {
    Info {
        pid: 999_999_999,
        process_start: 1,
        pane_id: None,
    }
}
fn receipt(conversation: &str, phase: &str, detail: Option<&str>) -> resume::Receipt {
    resume::Receipt {
        version: 1,
        request: resume::Request {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            expected_generation: "g".into(),
            account_id: "codex:a".into(),
            name: "agent-1".into(),
            host: host(),
            space: launch::Space::Existing {
                id: "inline".into(),
            },
            directory: None,
            confirm_unfinished: false,
            permissions: None,
        },
        directory: "/tmp".into(),
        saved_directory: None,
        created_directories: vec![],
        previous_name: "agent-1".into(),
        conversation_id: conversation.into(),
        generation: "g".into(),
        phase: phase.into(),
        detail: detail.map(str::to_owned),
        pane_reused: false,
        already_running: false,
        resumed_from: None,
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        operation_id: None,
        abandoned_from: None,
        abandoned_at: None,
        created_at: Some(daemon::now()),
        updated_at: daemon::now(),
        inline: Some(dead()),
    }
}

#[test]
fn a_resume_that_never_attached_settles_failed_and_unblocks_the_session() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let conversation = uuid::Uuid::new_v4().to_string();
    let mut r = receipt(&conversation, "starting_provider", None);
    let id = r.request.request_id.clone();
    resume::save(&paths, &mut r).unwrap();
    // Reading the session's operations settles the dead inline resume.
    assert_eq!(
        lifecycle::pending(&paths, &conversation, None).unwrap(),
        None
    );
    let settled = resume::read(&paths, &id).unwrap();
    assert_eq!(settled.phase, "failed");
    assert_eq!(settled.detail.as_deref(), Some(NEVER_ATTACHED));
    lifecycle::ensure_available(&paths, &conversation, None).unwrap();
}

#[test]
fn a_legacy_uncertain_never_attached_resume_settles_failed_but_other_uncertain_stays() {
    let d = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(d.path().join("state"))).unwrap();
    let conversation = uuid::Uuid::new_v4().to_string();
    let mut legacy = receipt(&conversation, "uncertain", Some(NEVER_ATTACHED_LEGACY));
    let mut other = receipt(
        &conversation,
        "uncertain",
        Some("The resume was dispatched"),
    );
    let (legacy_id, other_id) = (
        legacy.request.request_id.clone(),
        other.request.request_id.clone(),
    );
    resume::save(&paths, &mut legacy).unwrap();
    resume::save(&paths, &mut other).unwrap();
    // The unrelated uncertain resume still blocks, and is named with its id.
    let blocked = lifecycle::pending_detail(&paths, &conversation, None)
        .unwrap()
        .unwrap();
    assert_eq!(blocked.id, other_id);
    assert_eq!(
        (blocked.kind, blocked.phase.as_str()),
        ("resume", "uncertain")
    );
    let error = lifecycle::ensure_available(&paths, &conversation, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("RECOVERY_UNRESOLVED: operation "),
        "{error}"
    );
    assert!(error.contains(&other_id), "{error}");
    assert_eq!(resume::read(&paths, &legacy_id).unwrap().phase, "failed");
    // An in-progress phase keeps the retryable code, still with the id.
    let mut running = receipt(&uuid::Uuid::new_v4().to_string(), "dispatching", None);
    let (running_id, running_conversation) = (
        running.request.request_id.clone(),
        running.conversation_id.clone(),
    );
    running.inline = None;
    resume::save(&paths, &mut running).unwrap();
    let error = lifecycle::ensure_available(&paths, &running_conversation, None)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("SESSION_OPERATION_IN_PROGRESS: operation "),
        "{error}"
    );
    assert!(error.contains(&running_id), "{error}");
}
