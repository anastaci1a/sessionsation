use super::*;
#[test]
fn tab_labels_with_the_pane_space_prefix_read_as_the_bare_name() {
    for (label, space, expected) in [
        ("LEAD", Some("localica"), Some("LEAD")),
        ("Lead", Some("localica"), Some("Lead")),
        ("localica:LEAD", Some("localica"), Some("LEAD")),
        ("Localica:LEAD", Some("localica"), Some("LEAD")),
        ("other:LEAD", Some("localica"), None),
        ("a:b:c", Some("a"), None),
        (":x", Some("localica"), None),
        ("x:", Some("x"), None),
        ("localica:LEAD", None, None),
    ] {
        assert_eq!(host_name(label, space).ok(), expected, "{label}");
    }
    let v = serde_json::json!({"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","label":"old"}],
        "tabs":[{"tab_id":"w1:t1","label":"localica:LEAD"}],"workspaces":[{"workspace_id":"w1","label":"localica"}]});
    let o = tab(&v, "herdr/test/w1:p1").unwrap();
    assert_eq!((o.space, o.pane_label), (Some("localica"), Some("old")));
}
#[test]
fn host_observations_set_but_never_clear_a_space() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let s = record_launch(&p, "fixture", "t1", "LEAD", None).unwrap();
    // First observation of a Space: recorded without a new generation.
    apply(
        &p,
        vec![(s.clone(), Some("LEAD".into()), Some("localica".into()))],
    )
    .unwrap();
    let seen = list(&p).unwrap().remove(0);
    assert_eq!(seen.canonical().as_deref(), Some("localica:LEAD"));
    assert_eq!(seen.generation(), s.generation());
    // An invalid host label keeps the name; an unobserved Space keeps the Space.
    apply(&p, vec![(seen.clone(), None, None)]).unwrap();
    let kept = list(&p).unwrap().remove(0);
    assert_eq!(kept.canonical().as_deref(), Some("localica:LEAD"));
    assert_eq!(kept.generation(), seen.generation());
    // A live session placed in another Space moved (host wins): new generation.
    apply(
        &p,
        vec![(
            kept.clone(),
            Some("LEAD".into()),
            Some("sessionstuffs".into()),
        )],
    )
    .unwrap();
    let moved = list(&p).unwrap().remove(0);
    assert_eq!(moved.canonical().as_deref(), Some("sessionstuffs:LEAD"));
    assert_ne!(moved.generation(), kept.generation());
    // A stale observation never undoes it.
    apply(
        &p,
        vec![(kept, Some("LEAD".into()), Some("localica".into()))],
    )
    .unwrap();
    assert_eq!(list(&p).unwrap()[0].space.as_deref(), Some("sessionstuffs"));
}
#[test]
fn host_labels_are_identity_mapped_and_not_generated_from_titles() {
    let v = serde_json::json!({"panes":[{"pane_id":"w1:p1","tab_id":"w1:t2"}],"tabs":[{"tab_id":"w1:t1","label":"wrong"},{"tab_id":"w1:t2","label":"host name"}]});
    {
        let o = tab(&v, "herdr/test/w1:p1").unwrap();
        assert_eq!((o.tab_id, o.label, o.space), ("w1:t2", "host name", None));
    }
    assert!(tab(&v, "herdr/test/w1:p2").is_err());
}
#[test]
fn host_duplicates_remain_labels_and_unchanged_reads_do_not_change_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let a = record_launch(
        &p,
        "fixture",
        &uuid::Uuid::new_v4().to_string(),
        "one",
        None,
    )
    .unwrap();
    let b = record_launch(
        &p,
        "fixture",
        &uuid::Uuid::new_v4().to_string(),
        "two",
        None,
    )
    .unwrap();
    apply(
        &p,
        vec![
            (a, Some("same".into()), None),
            (b, Some("same".into()), None),
        ],
    )
    .unwrap();
    let before = list(&p).unwrap();
    assert_ne!(before[0].id, before[1].id);
    assert!(before.iter().all(|s| s.name == "same"));
    assert_eq!(
        super::super::rename(&p, &before[0].id, &before[0].generation(), "same")
            .unwrap()
            .generation(),
        before[0].generation()
    );
    apply(
        &p,
        before
            .iter()
            .cloned()
            .map(|s| (s, Some("same".into()), None))
            .collect(),
    )
    .unwrap();
    assert_eq!(
        before.iter().map(Session::generation).collect::<Vec<_>>(),
        list(&p)
            .unwrap()
            .iter()
            .map(Session::generation)
            .collect::<Vec<_>>()
    );
}
#[test]
fn observed_labels_survive_exit_but_stale_observations_never_undo_a_rename() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let s = record_launch(
        &p,
        "fixture",
        &uuid::Uuid::new_v4().to_string(),
        "old",
        None,
    )
    .unwrap();
    apply(&p, vec![(s.clone(), Some("HERDR label".into()), None)]).unwrap();
    let current = list(&p).unwrap().remove(0);
    assert_eq!(current.id, s.id);
    assert_eq!(current.conversation_id, s.conversation_id);
    assert_eq!(current.name, "HERDR label");
    apply(&p, vec![(s, Some("stale".into()), None)]).unwrap();
    assert_eq!(list(&p).unwrap()[0].name, "HERDR label");
    assert_eq!(reconcile(&p, &[]).unwrap()[0].name, "HERDR label");
}
fn claude_session(p: &Paths, label: &str, attachment: Option<Attachment>) -> Session {
    record_provider_launch(
        p,
        ProviderId::Claude,
        "fixture",
        &uuid::Uuid::new_v4().to_string(),
        label,
        Some("/work"),
        attachment,
    )
    .unwrap()
}
#[test]
fn claude_host_labels_use_the_codex_observation_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let a = claude_session(&p, "one", None);
    let b = record_launch(
        &p,
        "fixture",
        &uuid::Uuid::new_v4().to_string(),
        "two",
        None,
    )
    .unwrap();
    // Host labels are presentation: shared across providers, never suffixed.
    apply(
        &p,
        vec![
            (a.clone(), Some("same".into()), None),
            (b, Some("same".into()), None),
        ],
    )
    .unwrap();
    let before = list(&p).unwrap();
    assert!(before.iter().all(|s| s.name == "same"));
    apply(
        &p,
        before
            .iter()
            .cloned()
            .map(|s| (s, Some("same".into()), None))
            .collect(),
    )
    .unwrap();
    assert_eq!(
        before.iter().map(Session::generation).collect::<Vec<_>>(),
        list(&p)
            .unwrap()
            .iter()
            .map(Session::generation)
            .collect::<Vec<_>>(),
        "unchanged reads do not change generation"
    );
    let current = list(&p)
        .unwrap()
        .into_iter()
        .find(|s| s.id == a.id)
        .unwrap();
    apply(
        &p,
        vec![(current.clone(), Some("HERDR label".into()), None)],
    )
    .unwrap();
    let renamed = list(&p)
        .unwrap()
        .into_iter()
        .find(|s| s.id == a.id)
        .unwrap();
    assert_eq!(renamed.name, "HERDR label");
    assert_eq!(renamed.conversation_id, a.conversation_id);
    assert_eq!(renamed.directory.as_deref(), Some("/work"));
    apply(&p, vec![(current, Some("stale".into()), None)]).unwrap();
    let kept = list(&p)
        .unwrap()
        .into_iter()
        .find(|s| s.id == a.id)
        .unwrap();
    assert_eq!(
        kept.name, "HERDR label",
        "stale observations never undo a rename"
    );
}
#[test]
fn claude_sessions_are_live_only_through_their_exact_verified_attachment() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let pid = std::process::id();
    let (_, start) = crate::provider::claude::proc_start(pid).unwrap();
    let attach = |start| Attachment {
        host: "herdr".into(),
        target: "herdr/test/w1:p1".into(),
        pid,
        process_start: start,
    };
    let live = claude_session(&p, "live", Some(attach(start)));
    assert!(attached_live(&live, &[]));
    let reused = claude_session(&p, "reused", Some(attach(start + 1)));
    assert!(!attached_live(&reused, &[]), "PID reuse is not liveness");
    let detached = claude_session(&p, "detached", None);
    assert!(!attached_live(&detached, &[]));
    // Codex keeps requiring a discovered instance, never a bare PID.
    let codex = record_launch(
        &p,
        "fixture",
        &uuid::Uuid::new_v4().to_string(),
        "codex",
        Some(attach(start)),
    )
    .unwrap();
    assert!(!attached_live(&codex, &[]));
}
#[test]
fn gemini_labels_and_liveness_follow_the_native_provider_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let pid = std::process::id();
    let (_, start) = crate::provider::claude::proc_start(pid).unwrap();
    let attach = |start| Attachment {
        host: "herdr".into(),
        target: "herdr/test/w1:p1".into(),
        pid,
        process_start: start,
    };
    let gemini = |label: &str, attachment| {
        record_provider_launch(
            &p,
            ProviderId::Gemini,
            "fixture",
            &uuid::Uuid::new_v4().to_string(),
            label,
            Some("/work"),
            attachment,
        )
        .unwrap()
    };
    let live = gemini("live", Some(attach(start)));
    assert!(attached_live(&live, &[]));
    assert!(!attached_live(
        &gemini("reused", Some(attach(start + 1))),
        &[]
    ));
    assert!(!attached_live(&gemini("detached", None), &[]));
    // A HERDR tab label becomes the catalog label (presentation only);
    // stale observations never undo it and identity is unchanged.
    apply(&p, vec![(live.clone(), Some("HERDR tab".into()), None)]).unwrap();
    let renamed = list(&p)
        .unwrap()
        .into_iter()
        .find(|s| s.id == live.id)
        .unwrap();
    assert_eq!(renamed.name, "HERDR tab");
    assert_eq!(renamed.conversation_id, live.conversation_id);
    assert_eq!(renamed.directory.as_deref(), Some("/work"));
    apply(&p, vec![(live.clone(), Some("stale".into()), None)]).unwrap();
    let kept = list(&p)
        .unwrap()
        .into_iter()
        .find(|s| s.id == live.id)
        .unwrap();
    assert_eq!(kept.name, "HERDR tab");
}
