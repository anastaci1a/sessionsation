#[test]
fn history_readers_do_not_block_resume_but_writers_do() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(&path, b"fixture").unwrap();
    let reader = std::fs::File::open(&path).unwrap();
    assert!(!super::super::open_rollouts().unwrap().contains(&path));
    let writer = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    assert!(super::super::open_rollouts().unwrap().contains(&path));
    drop(writer);
    assert!(!super::super::open_rollouts().unwrap().contains(&path));
    drop(reader);
    assert_eq!(
        super::super::descriptor_writable("flags:\t0100000\n"),
        Some(false)
    );
    assert_eq!(
        super::super::descriptor_writable("flags:\t0100002\n"),
        Some(true)
    );
    assert_eq!(super::super::descriptor_writable("flags: invalid"), None);
}
use super::super::*;
#[test]
fn preserved_archive_is_a_resume_source_without_recreating_account_credentials() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("vault/home");
    fs::create_dir_all(home.join("archived_sessions")).unwrap();
    let rollout = home.join("archived_sessions/saved.jsonl");
    fs::write(
        &rollout,
        format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"saved","cwd":d.path(),"source":"cli"}})
        ),
    )
    .unwrap();
    assert!(descendants(&home, "saved").is_err());
    fs::write(home.parent().unwrap().join("history-checks.json"), "[]").unwrap();
    assert_eq!(descendants(&home, "saved").unwrap(), vec![rollout]);
    assert!(!home.join("auth.json").exists());
}
#[test]
fn activity_recovers_after_old_damage_but_never_trusts_a_damaged_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("rollout.jsonl");
    let event = |kind| format!("{}\n", json!({"type":"event_msg","payload":{"type":kind}}));
    fs::write(
        &file,
        format!("broken old record\n{}", event("task_started")),
    )
    .unwrap();
    assert_eq!(rollout_activity(&file), Activity::Busy);
    assert!(!complete_rollout(&file).unwrap());
    fs::write(
        &file,
        format!(
            "broken old record\n{}{}",
            event("task_started"),
            event("task_complete")
        ),
    )
    .unwrap();
    assert_eq!(rollout_activity(&file), Activity::Idle);
    assert!(complete_rollout(&file).unwrap());
    fs::write(&file, format!("{}broken tail", event("task_complete"))).unwrap();
    assert_eq!(rollout_activity(&file), Activity::Unknown);
    assert!(!complete_rollout(&file).unwrap());
    fs::write(
        &file,
        format!("{}{}", event("task_started"), event("turn_aborted")),
    )
    .unwrap();
    assert_eq!(rollout_activity(&file), Activity::Idle);
}
#[test]
fn activity_reads_a_bounded_tail_and_never_infers_idle_from_an_old_prefix() {
    use std::{cell::Cell, io::Cursor, rc::Rc};
    struct Counted {
        inner: Cursor<Vec<u8>>,
        count: Rc<Cell<usize>>,
    }
    impl Read for Counted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.count.set(self.count.get() + n);
            Ok(n)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }
    let event = |kind| format!("{}\n", json!({"type":"event_msg","payload":{"type":kind}}));
    let mut data = event("task_complete").into_bytes();
    data.extend(vec![b'x'; ACTIVITY_TAIL_BYTES as usize * 8]);
    data.extend_from_slice(b"\n");
    let count = Rc::new(Cell::new(0));
    assert_eq!(
        activity_from_tail(Counted {
            inner: Cursor::new(data.clone()),
            count: count.clone()
        }),
        Activity::Unknown
    );
    assert!(count.get() <= ACTIVITY_TAIL_BYTES as usize);
    data.extend_from_slice(event("task_started").as_bytes());
    assert_eq!(
        activity_from_tail(Cursor::new(data.clone())),
        Activity::Busy
    );
    data.extend_from_slice(event("task_complete").as_bytes());
    assert_eq!(
        activity_from_tail(Cursor::new(data.clone())),
        Activity::Idle
    );
    data.extend_from_slice(b"broken tail");
    assert_eq!(activity_from_tail(Cursor::new(data)), Activity::Unknown);
}
use crate::model::{Account, State};

#[test]
fn codex_threads_and_pane_commands_carry_the_catalog_session_id() {
    // Contract 1.62.0 (SM7).
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("root"))).unwrap();
    let thread = "11111111-1111-4111-8111-111111111111";
    let home = tmp.path().join("codex home");
    let options = vec!["--yolo".to_owned()];
    let plain = resume_shell(
        &home,
        "codex",
        &options,
        thread,
        &tmp.path().join("missing.jsonl"),
        LaunchPermissions::Default,
    );
    // No catalog entry: nothing to name.
    assert_eq!(with_session_env(&paths, thread, plain.clone()), plain);
    let mut params = json!({"threadId":thread,"config":{"model_reasoning_effort":"high"}});
    set_known_thread_session_env(&paths, &mut params, thread);
    assert!(
        params["config"]
            .get("shell_environment_policy.set.SESSIONSATION_SESSION_ID")
            .is_none()
    );
    let session = crate::catalog::record_provider_launch_as(
        &paths,
        crate::provider::ProviderId::Codex,
        "personal",
        thread,
        "label",
        None,
        None,
        Some("22222222-2222-4222-8222-222222222222"),
    )
    .unwrap();
    assert_eq!(session.id, "22222222-2222-4222-8222-222222222222");
    let named = with_session_env(&paths, thread, plain.clone());
    assert!(
        named.starts_with(
            "exec env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN SESSIONSATION_SESSION_ID='22222222-2222-4222-8222-222222222222' CODEX_HOME="
        ),
        "{named}"
    );
    assert_eq!(
        named.replacen(
            "SESSIONSATION_SESSION_ID='22222222-2222-4222-8222-222222222222' ",
            "",
            1
        ),
        plain
    );
    let remote = with_session_env(
        &paths,
        thread,
        super::super::movement::remote_resume_shell(&home, "codex", &options, thread),
    );
    assert!(
        remote.contains(
            " SESSIONSATION_SESSION_ID='22222222-2222-4222-8222-222222222222' CODEX_HOME="
        ),
        "{remote}"
    );
    set_known_thread_session_env(&paths, &mut params, thread);
    assert_eq!(
        params["config"],
        json!({"model_reasoning_effort":"high",
            "shell_environment_policy.set.SESSIONSATION_SESSION_ID":"22222222-2222-4222-8222-222222222222"})
    );
    // A second record of the same conversation keeps its entry's id.
    let again = crate::catalog::record_provider_launch_as(
        &paths,
        crate::provider::ProviderId::Codex,
        "personal",
        thread,
        "label",
        None,
        None,
        Some("33333333-3333-4333-8333-333333333333"),
    )
    .unwrap();
    assert_eq!(again.id, session.id);
}

#[test]
fn codex_start_permissions_follow_the_launch_mode() {
    let saved = Some(("on-request".to_owned(), "workspace-write".to_owned()));
    assert_eq!(
        start_permissions(LaunchPermissions::Yolo, saved.clone()),
        Some(("never".into(), "danger-full-access".into()))
    );
    assert_eq!(
        start_permissions(LaunchPermissions::Default, saved.clone()),
        saved
    );
    assert_eq!(start_permissions(LaunchPermissions::Default, None), None);
    let mut params = json!({"cwd":"/","approvalPolicy":"never","sandbox":"danger-full-access"});
    set_permissions(&mut params, None);
    assert_eq!(params, json!({"cwd":"/"}));
    set_permissions(&mut params, saved);
    assert_eq!(params["approvalPolicy"], "on-request");
    assert_eq!(params["sandbox"], "workspace-write");
}

#[test]
fn moved_session_uses_latest_recorded_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let rollout = tmp.path().join("rollout.jsonl");
    fs::write(&rollout, concat!(
        "{\"type\":\"turn_context\",\"payload\":{\"approval_policy\":\"never\",\"sandbox_policy\":{\"type\":\"danger-full-access\"}}}\n",
        "{\"type\":\"turn_context\",\"payload\":{\"approval_policy\":\"on-request\",\"sandbox_policy\":{\"type\":\"workspace-write\"}}}\n"
    )).unwrap();
    let command = resume_shell(
        tmp.path(),
        "codex",
        &[
            "--dangerously-bypass-approvals-and-sandbox".into(),
            "--model".into(),
            "test-model".into(),
        ],
        "thread",
        &rollout,
        LaunchPermissions::Default,
    );
    assert!(command.contains("'--ask-for-approval' 'on-request' '--sandbox' 'workspace-write'"));
    assert!(command.contains("'--model' 'test-model'"));
    assert!(!command.contains("'--dangerously-bypass-approvals-and-sandbox'"));
    // Launch permissions `yolo` (the default) grant full access on restart/move.
    let yolo = resume_shell(
        tmp.path(),
        "codex",
        &[],
        "thread",
        &rollout,
        LaunchPermissions::Yolo,
    );
    assert!(yolo.contains("'--ask-for-approval' 'never' '--sandbox' 'danger-full-access'"));
}

#[test]
fn moved_session_uses_explicit_flags_then_launch_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let rollout = tmp.path().join("rollout.jsonl");
    fs::write(&rollout, "{\"type\":\"session_meta\",\"payload\":{}}\n").unwrap();
    let known = resume_shell(
        tmp.path(),
        "codex",
        &[
            "--sandbox".into(),
            "read-only".into(),
            "--ask-for-approval".into(),
            "on-request".into(),
        ],
        "thread",
        &rollout,
        LaunchPermissions::Default,
    );
    assert!(known.contains("'--ask-for-approval' 'on-request' '--sandbox' 'read-only'"));
    let unknown = resume_shell(
        tmp.path(),
        "codex",
        &[],
        "thread",
        &rollout,
        LaunchPermissions::Yolo,
    );
    assert!(unknown.contains("'--ask-for-approval' 'never' '--sandbox' 'danger-full-access'"));
    // `default` with nothing recorded leaves Codex's own configuration in charge.
    let provider = resume_shell(
        tmp.path(),
        "codex",
        &[],
        "thread",
        &rollout,
        LaunchPermissions::Default,
    );
    assert!(!provider.contains("--ask-for-approval") && !provider.contains("--sandbox"));
}

#[test]
fn external_home_display_requires_unique_current_enrolled_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let external = tmp.path().join("default-codex");
    private_dir(&external).unwrap();
    let personal = paths.account("personal").unwrap();
    let work = paths.account("work").unwrap();
    private_dir(&personal).unwrap();
    private_dir(&work).unwrap();
    fs::write(
        external.join("auth.json"),
        r#"{"tokens":{"account_id":"id-personal"}}"#,
    )
    .unwrap();
    fs::write(
        personal.join("auth.json"),
        r#"{"tokens":{"account_id":"id-personal"}}"#,
    )
    .unwrap();
    fs::write(
        work.join("auth.json"),
        r#"{"tokens":{"account_id":"id-work"}}"#,
    )
    .unwrap();
    let mut state = State::default();
    state.accounts.insert(
        "personal".into(),
        Account::new("personal".into(), "id-personal".into()),
    );
    state
        .accounts
        .insert("work".into(), Account::new("work".into(), "id-work".into()));
    assert_eq!(account_for_home(&paths, &external), "external");
    assert_eq!(
        display_account_for_home(&paths, &external, "external", Some(&state)).as_deref(),
        Some("personal")
    );
    assert_eq!(
        display_account_for_home(&paths, &work, "work", Some(&state)).as_deref(),
        Some("work")
    );
    fs::write(
        external.join("auth.json"),
        r#"{"tokens":{"account_id":"unregistered"}}"#,
    )
    .unwrap();
    assert_eq!(
        display_account_for_home(&paths, &external, "external", Some(&state)),
        None
    );
    fs::write(
        external.join("auth.json"),
        r#"{"tokens":{"account_id":"id-personal"}}"#,
    )
    .unwrap();
    fs::write(
        personal.join("auth.json"),
        r#"{"tokens":{"account_id":"changed"}}"#,
    )
    .unwrap();
    assert_eq!(
        display_account_for_home(&paths, &external, "external", Some(&state)),
        None
    );
    fs::write(
        personal.join("auth.json"),
        r#"{"tokens":{"account_id":"id-personal"}}"#,
    )
    .unwrap();
    state.accounts.get_mut("work").unwrap().identity = "id-personal".into();
    fs::write(
        work.join("auth.json"),
        r#"{"tokens":{"account_id":"id-personal"}}"#,
    )
    .unwrap();
    assert_eq!(
        display_account_for_home(&paths, &external, "external", Some(&state)),
        None
    );
    assert_eq!(
        display_account_for_home(&paths, &external, "external", None),
        None
    );
}
#[test]
fn inactive_discovery_prefers_longest_consistent_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    crate::store::save_config(&paths, &crate::config::Config::default()).unwrap();
    let id = "33333333-3333-4333-8333-333333333333";
    let base = format!(
        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"source\":\"cli\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n",
        tmp.path().display()
    );
    for (account, content) in [
        ("first", base.clone()),
        (
            "second",
            format!(
                "{base}{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\"}}}}\n"
            ),
        ),
    ] {
        let dir = paths.account(account).unwrap().join("sessions/2026/10/09");
        private_dir(&dir).unwrap();
        fs::write(dir.join(format!("rollout-{id}.jsonl")), content).unwrap();
    }
    let unfinished = "55555555-5555-4555-8555-555555555555";
    fs::write(
        paths.root.join(format!("accounts/codex/first/sessions/2026/10/09/rollout-{unfinished}.jsonl")),
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{unfinished}\",\"source\":\"cli\",\"cwd\":\"{}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\"}}}}\n", tmp.path().display()),
    ).unwrap();
    let found = all_instances(&paths).unwrap();
    assert_eq!(found.len(), 2);
    let latest = found.iter().find(|item| item.thread_id == id).unwrap();
    assert!(latest.inactive);
    assert_eq!(latest.home, paths.account("second").unwrap());
    assert_eq!(
        found
            .iter()
            .find(|item| item.thread_id == unfinished)
            .unwrap()
            .block_reason
            .as_deref(),
        Some("unfinished")
    );
    let mut prior = Job {
        resume_destination: None,
        resume_directory: None,
        non_steering: false,
        id: "prior".into(),
        kind: "account".into(),
        source: latest.clone(),
        target: "second".into(),
        phase: "running_on_target".into(),
        detail: String::new(),
        source_lease_released: false,
        pause_approved: false,
        risks_approved: false,
        unfinished_history_confirmed: false,
        pause_delivery: String::new(),
        resume_after_move: false,
        continuation_delivery: String::new(),
        continuation_turn_id: None,
        continuation_observation: String::new(),
        source_archived: false,
        created_at: 0,
        expires_at: 0,
        confirmation_seq: 0,
        acknowledged_risks: Vec::new(),
        target_loaded: false,
        steered_turns: Default::default(),
        relocation: None,
        termination_threads: Vec::new(),
        history_checks: Vec::new(),
        prepared_threads: Vec::new(),
        permissions: None,
        close_pane: false,
        pane_closed: None,
        pane_close_reason: None,
    };
    prior.source.pane = "%29".into();
    let mut live = latest.clone();
    live.inactive = false;
    live.pane = "%29".into();
    assert!(!job_conflicts_with_source(&paths, &prior, &live));
    prior.phase = "waiting_for_idle".into();
    assert!(job_conflicts_with_source(&paths, &prior, &live));
}
#[test]
fn repeated_handoff_advances_only_from_an_older_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let dest = tmp.path().join("dest");
    let relative = Path::new("sessions/2026/10/09/rollout-test.jsonl");
    private_dir(source.join("sessions/2026/10/09").as_path()).unwrap();
    private_dir(dest.join("sessions/2026/10/09").as_path()).unwrap();
    let old = b"first\n";
    let newer = b"first\nsecond\n";
    fs::write(source.join(relative), newer).unwrap();
    fs::write(dest.join(relative), old).unwrap();
    copy_rollout(&source.join(relative), &source, &dest).unwrap();
    assert_eq!(fs::read(dest.join(relative)).unwrap(), newer);
    fs::write(source.join(relative), b"diverged\n").unwrap();
    assert!(copy_rollout(&source.join(relative), &source, &dest).is_err());
    assert_eq!(fs::read(dest.join(relative)).unwrap(), newer);
}
