use super::super::*;
#[test]
fn resume_cwd_requires_absolute_existing_directory() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        resume_cwd(tmp.path().to_str().unwrap()).unwrap(),
        tmp.path()
    );
    for bad in ["", "relative", "/nonexistent/sessionsation-dir"] {
        let e = resume_cwd(bad).unwrap_err().to_string();
        assert!(e.starts_with("SAVED_DIRECTORY_UNAVAILABLE"), "{e}");
    }
    let f = tmp.path().join("f");
    std::fs::write(&f, "").unwrap();
    assert!(resume_cwd(f.to_str().unwrap()).is_err());
}
#[test]
fn attachment_lookup_is_read_only_and_checks_catalog_and_process_epochs() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let pid = std::process::id();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let epoch: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    for host in ["herdr", "tmux"] {
        let s = catalog::record_launch(
            &p,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            host,
            Some(catalog::Attachment {
                host: host.into(),
                target: format!("{host}/fixture/target"),
                pid,
                process_start: epoch,
            }),
        )
        .unwrap();
        let before = std::fs::read(
            p.root
                .join(crate::layout::SESSION_CATALOG)
                .join("catalog.json"),
        )
        .unwrap();
        let result = attachment(&p, &s.id, &s.generation()).unwrap();
        assert_eq!(result["id"], s.id);
        assert_eq!(result["attachment"]["host"], host);
        assert_eq!(result["starts_process"], false);
        assert_eq!(result["sends_input"], false);
        assert_eq!(
            std::fs::read(
                p.root
                    .join(crate::layout::SESSION_CATALOG)
                    .join("catalog.json")
            )
            .unwrap(),
            before
        );
        catalog::rename(&p, &s.id, &s.generation(), &format!("{host}-renamed")).unwrap();
        assert!(
            attachment(&p, &s.id, &s.generation())
                .unwrap_err()
                .to_string()
                .contains("STALE_TARGET")
        );
    }
    let s = catalog::record_launch(
        &p,
        "fixture",
        "wrong-epoch",
        "Wrong",
        Some(catalog::Attachment {
            host: "herdr".into(),
            target: "herdr/fixture/target".into(),
            pid,
            process_start: epoch + 1,
        }),
    )
    .unwrap();
    assert!(
        attachment(&p, &s.id, &s.generation())
            .unwrap_err()
            .to_string()
            .contains("SESSION_ATTACHMENT_UNAVAILABLE")
    );
    let s = catalog::record_launch(&p, "fixture", "saved", "Saved", None).unwrap();
    assert!(
        attachment(&p, &s.id, &s.generation())
            .unwrap_err()
            .to_string()
            .contains("SESSION_NOT_ATTACHED")
    );
    assert!(!p.root.join(crate::layout::LAUNCHES).exists());
}
#[test]
fn rows_and_capabilities_carry_the_space_scoped_names() {
    let c = capabilities();
    assert_eq!(
        c["session_names"],
        json!({"version":1,"live_herdr_source":"host_tab_label","retained_when_unobserved":true,"host_labels_unique":false,"rename_updates_host":true})
    );
    assert_eq!(c["session_spaces"]["since_contract"], "1.48.0");
    assert_eq!(c["contract_version"], "1.73.5");
    assert_eq!(c["send_request"]["since_contract"], "1.64.0");
    let root = &c["root_alias"];
    assert_eq!(root["since_contract"], "1.54.0");
    assert_eq!(root["layout_version"], 3);
    assert_eq!(
        root["gate"]["values"],
        json!(["pass", "fail", "not_applicable"])
    );
    let reset = &c["account_reset_credit"];
    assert_eq!(reset["since_contract"], "1.53.0");
    assert_eq!(reset["providers"], json!(["codex", "claude"]));
    assert_eq!(reset["method"], "account/rateLimitResetCredit/consume");
    assert_eq!(reset["idempotent_by"], "request_id");
    assert_eq!(
        reset["outcomes"],
        json!(["reset", "nothingToReset", "noCredit", "alreadyRedeemed"])
    );
    let from = &c["session_spaces"]["from_component"];
    assert_eq!(from["since_contract"], "1.50.0");
    assert_eq!(from["changed_in"]["contract"], "1.51.0");
    assert_eq!(from["from_name"], "<component>");
    assert_eq!(from["collision_fallback"], "sessionsation:<component>");
    assert_eq!(
        from["prefixed_fact"],
        json!({"field":"from_component_prefixed","value":"collides_with_session"})
    );
    let mut s = catalog::Session {
        id: "stable".into(),
        name: "LEAD".into(),
        provider: "codex".into(),
        account: "selected".into(),
        conversation_id: "thread".into(),
        revision: 1,
        attachment: None,
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
    let unscoped = public_session(s.clone());
    assert!(unscoped.get("space").is_some_and(Value::is_null));
    assert!(unscoped["canonical"].is_null());
    s.space = Some("localica".into());
    let scoped = public_session(s.clone());
    assert_eq!(
        (
            scoped["space"].as_str(),
            scoped["canonical"].as_str(),
            scoped["name"].as_str()
        ),
        (Some("localica"), Some("localica:LEAD"), Some("LEAD"))
    );
    assert_eq!(
        scoped["generation"], unscoped["generation"],
        "space is not part of the generation"
    );
    s.space = Some("a:b".into());
    assert!(public_session(s)["canonical"].is_null());
}
#[test]
fn resume_destination_defaults_to_the_stored_space_and_recreates_it() {
    let list = || {
        Ok(vec![
            json!({"workspace_id":"w1","label":"Main"}),
            json!({"workspace_id":"w2","label":"localica"}),
        ])
    };
    let t = |space, new_space, create_space, name| ResumeTarget {
        space,
        new_space,
        create_space,
        name,
        owner: None,
    };
    let dest = |target: ResumeTarget<'_>, stored| resume_destination(list, &target, stored);
    let existing = |id: &str, label: &str| Destination {
        space: Some(id.into()),
        new_space: None,
        label: label.into(),
    };
    let fresh = |label: &str| Destination {
        space: None,
        new_space: Some(label.into()),
        label: label.into(),
    };
    // Stored Space exists: reopen it (case-insensitive label match).
    assert_eq!(
        dest(t(None, None, false, None), Some("LOCALICA"))
            .unwrap()
            .0,
        existing("w2", "localica")
    );
    // Stored Space is gone: recreate exactly that label, no --create-space needed.
    assert_eq!(
        dest(t(None, None, false, None), Some("liq-glass"))
            .unwrap()
            .0,
        fresh("liq-glass")
    );
    // An explicit placement wins.
    assert_eq!(
        dest(t(Some("w1"), None, false, None), Some("liq-glass"))
            .unwrap()
            .0,
        existing("w1", "Main")
    );
    assert_eq!(
        dest(t(None, Some("Main"), false, None), Some("x"))
            .unwrap()
            .0,
        existing("w1", "Main")
    );
    // Unscoped and unnamed: the unchanged default.
    assert_eq!(
        dest(t(None, None, false, None), None).unwrap().0,
        fresh("sessionsation")
    );
    // A canonical name: its Space when nothing else is given, else it must agree.
    let (d, n) = dest(t(None, None, false, Some("main:x")), None).unwrap();
    assert_eq!((d, n.as_deref()), (existing("w1", "Main"), Some("x")));
    let e = format!(
        "{:#}",
        dest(t(None, None, false, Some("Other:x")), Some("Main")).unwrap_err()
    );
    assert!(e.starts_with("SPACE_MISMATCH"), "{e}");
    let e = format!(
        "{:#}",
        dest(t(None, None, false, Some("nowhere:x")), None).unwrap_err()
    );
    assert!(e.starts_with("SPACE_NOT_FOUND"), "{e}");
    // --create-space: the name's Space, which must not exist.
    assert_eq!(
        dest(t(None, None, true, Some("newspace:LEAD")), None)
            .unwrap()
            .0,
        fresh("newspace")
    );
    let e = format!(
        "{:#}",
        dest(t(None, None, true, Some("Localica:LEAD")), None).unwrap_err()
    );
    assert!(e.starts_with("WORKSPACE_NAME_EXISTS"), "{e}");
    let e = format!(
        "{:#}",
        dest(t(None, None, true, Some("LEAD")), None).unwrap_err()
    );
    assert!(e.starts_with("INVALID_ARGUMENT"), "{e}");
    assert!(dest(t(None, None, false, Some("a:b:c")), None).is_err());
}
#[test]
fn resume_into_a_space_keeps_the_session_name_as_the_tab_label() {
    let s = catalog::Session {
        id: "stable".into(),
        name: "my-work".into(),
        provider: "codex".into(),
        account: "selected".into(),
        conversation_id: "thread".into(),
        revision: 1,
        attachment: None,
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
    // Without --name the existing name labels the tab (live HERDR names follow the
    // tab label), never the "sessionsation" default.
    for space in [Some("w1"), None] {
        let p = resume_placement(space, None, &s);
        assert_eq!(p.space, space);
        assert_eq!(p.label, Some("my-work"));
        assert!(p.strict);
    }
    let unnamed = catalog::Session {
        name: " ".into(),
        ..s
    };
    assert_eq!(resume_placement(Some("w1"), None, &unnamed).label, None);
}
#[test]
fn a_resume_name_is_unique_only_in_the_destination_space() {
    let t = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(t.path().join("root"))).unwrap();
    let mk = |name: &str, space: &str, conv: &str| {
        let s = catalog::record_launch(&paths, "fixture", conv, name, None).unwrap();
        catalog::set_space(&paths, &s.id, &s.generation(), Some(space)).unwrap()
    };
    let old = mk("src", "old", "c1");
    mk("foo", "old", "c2"); // exists only in the old space
    mk("bar", "x", "c3"); // exists in the destination
    // foo is free in X: renaming succeeds (not blocked by the old space's foo).
    let r = resume_named(&paths, old.clone(), &old.generation(), Some("foo"), "x").unwrap();
    assert_eq!(r.name, "foo");
    assert_eq!(
        r.space.as_deref(),
        Some("old"),
        "the Space is recorded after the start"
    );
    // bar exists in X (case-insensitive): refused.
    let e = resume_named(&paths, r.clone(), &r.generation(), Some("BAR"), "x").unwrap_err();
    assert!(format!("{e:#}").starts_with("SESSION_NAME_EXISTS"), "{e:#}");
}
#[test]
fn an_unobserved_attachment_is_not_an_inactive_history_source() {
    let s = catalog::Session {
        id: "stable".into(),
        name: "Name".into(),
        provider: "codex".into(),
        account: "selected".into(),
        conversation_id: "thread".into(),
        revision: 1,
        attachment: Some(catalog::Attachment {
            host: "herdr".into(),
            target: "herdr/named/w1:p1".into(),
            pid: 10,
            process_start: 20,
        }),
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
    let mut source: external::Instance = serde_json::from_value(json!({"inactive":true,"account":"selected","pane":"","tmux_session":"","pid":0,"cwd":"/tmp","thread_id":"thread","rollout":"/tmp/fixture","home":"/tmp/home","binary":"codex","resume_options":[]})).unwrap();
    assert!(!source_matches(&s, &source));
    source.inactive = false;
    source.pane = "herdr/named/w1:p1".into();
    source.pid = 10;
    source.start_time = 20;
    assert!(source_matches(&s, &source));
    source.start_time += 1;
    assert!(!source_matches(&s, &source));
}
#[test]
fn resume_reasons_explain_observed_safety_gates() {
    let mut source: external::Instance = serde_json::from_value(json!({
        "inactive":true,"pane":"","tmux_session":"","pid":0,"cwd":"/tmp",
        "thread_id":"fixture","rollout":"/tmp/history","home":"/tmp/home",
        "binary":"codex","resume_options":[]
    }))
    .unwrap();
    assert_eq!(
        resume_block_reason(false, Some(&source)),
        Some("unsupported_provider")
    );
    assert_eq!(resume_block_reason(true, None), Some("history_unavailable"));
    assert_eq!(resume_block_reason(true, Some(&source)), None);
    for (block, expected) in [
        ("unknown", "missing_directory"),
        ("busy", "history_busy"),
        ("unfinished", "unfinished_history"),
        ("diverged", "conflicting_history"),
        ("moving", "moving"),
        ("future", "unverified_history"),
    ] {
        source.block_reason = Some(block.into());
        assert_eq!(resume_block_reason(true, Some(&source)), Some(expected));
    }
    source.block_reason = None;
    source.move_id = Some("operation".into());
    assert_eq!(resume_block_reason(true, Some(&source)), Some("moving"));
    source.inactive = false;
    assert_eq!(
        resume_block_reason(true, Some(&source)),
        Some("already_running")
    );
}
#[test]
fn session_elapsed_uses_current_attachment_clock_and_rejects_unknown_ticks() {
    assert_eq!(attachment_started_at(1000, 120., 6000, 100), Some(940));
    assert_eq!(attachment_started_at(1000, 120., 0, 100), None);
    assert_eq!(attachment_started_at(1000, 120., 13000, 100), None);
    assert_eq!(attachment_started_at(1000, 120., 6000, 0), None);
}
#[test]
fn stale_thresholds_and_credits_do_not_auto_select() {
    let mut a = json!({"selectable":true,"usage_fresh":true,"switch_thresholds":{"needs_plan_review":false,"short_percent":90.,"weekly_percent":90.},"usage":{"observed_at":100,"short":{"used_percent":20.,"resets_at":500},"weekly":{"used_percent":20.,"resets_at":500}}});
    assert_eq!(account_prefill(&a, 101, 210)["eligible"], true);
    a["usage"]["short"]["used_percent"] = json!(90.);
    assert_eq!(account_prefill(&a, 101, 210)["eligible"], false);
    assert_eq!(a["selectable"], true);
    a["usage"]["short"]["used_percent"] = json!(20.);
    a["usage"]["credits"] = json!({"has_credits":true});
    a["usage"]["exhausted"] = json!(true);
    assert_eq!(account_prefill(&a, 101, 210)["eligible"], false);
    a["usage"]["exhausted"] = json!(false);
    assert_eq!(account_prefill(&a, 311, 210)["eligible"], false);
}
