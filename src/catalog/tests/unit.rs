use super::super::*;
fn seed(paths: &Paths) -> Session {
    let s = Session {
        id: uuid::Uuid::new_v4().to_string(),
        name: "first".into(),
        provider: "codex".into(),
        account: "selected".into(),
        conversation_id: uuid::Uuid::new_v4().to_string(),
        revision: 1,
        attachment: Some(Attachment {
            host: "herdr".into(),
            target: "herdr/test/w1:p1".into(),
            pid: 42,
            process_start: 99,
        }),
        directory: None,
        previous_conversation_ids: vec![],
        space: None,
    };
    transaction(paths, |c| {
        c.sessions.insert(s.id.clone(), s.clone());
        Ok(())
    })
    .unwrap();
    s
}
#[test]
fn names_are_thirty_characters_but_legacy_identity_is_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let s = seed(&p);
    transaction(&p, |c| {
        c.sessions.get_mut(&s.id).unwrap().name = "legacy-long-session-name-over-limit".into();
        Ok(())
    })
    .unwrap();
    let legacy = list(&p).unwrap().remove(0);
    assert_eq!(legacy.name, "legacy-long-session-name-over-limit");
    assert_eq!(
        resolve(&p, &legacy.id, &legacy.generation())
            .unwrap()
            .conversation_id,
        s.conversation_id
    );
    assert!(rename(&p, &s.id, &legacy.generation(), &"x".repeat(65)).is_err());
    let renamed = rename(&p, &s.id, &legacy.generation(), "界".repeat(64).as_str()).unwrap();
    assert_eq!(renamed.id, s.id);
    for conversation in ["a", "b"] {
        let assigned = record_launch(
            &p,
            "selected",
            conversation,
            "long-provider-session-label-that-exceeds-the-sixty-four-character-session-name-limit",
            None,
        )
        .unwrap();
        assert_eq!(assigned.name.chars().count(), 64);
    }
    assert_eq!(list(&p).unwrap().len(), 3);
}
#[test]
fn rename_is_atomic_unique_and_rejects_old_generations() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let s = seed(&p);
    let old = s.generation();
    let renamed = rename(&p, &s.id, &old, "renamed").unwrap();
    assert_eq!(renamed.id, s.id);
    assert_eq!(renamed.conversation_id, s.conversation_id);
    assert!(resolve(&p, &s.id, &old).is_err());
    assert!(rename(&p, &s.id, &old, "stale").is_err());
    let second = seed(&p);
    assert!(rename(&p, &second.id, &second.generation(), "renamed").is_err());
    assert_eq!(list(&p).unwrap().len(), 2);
}
#[test]
fn stable_identity_follows_account_and_runtime_replacements() {
    let t = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(t.path().join("data"))).unwrap();
    let before = seed(&p);
    let i:crate::external::Instance=serde_json::from_value(serde_json::json!({"pane":"herdr/test/w2:p3","tmux_session":"provider-label","pid":52,"start_time":999,"cwd":"/tmp","thread_id":before.conversation_id,"rollout":"/tmp/fixture","home":"/tmp/fixture-home","binary":"codex","resume_options":[],"account":"destination"})).unwrap();
    let after = reconcile(&p, &[i]).unwrap().remove(0);
    assert_eq!(before.id, after.id);
    assert_eq!(before.name, after.name);
    assert_eq!(before.conversation_id, after.conversation_id);
    assert_eq!(after.account, "destination");
    assert!(resolve(&p, &before.id, &before.generation()).is_err());
    assert_eq!(after.attachment.unwrap().pid, 52);
}
#[test]
fn concurrent_renames_cannot_claim_one_name() {
    let t = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(t.path().join("data"))).unwrap();
    let first = seed(&p);
    let first = rename(&p, &first.id, &first.generation(), "one").unwrap();
    let second = seed(&p);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers = [first, second]
        .into_iter()
        .map(|s| {
            let p = p.clone();
            let b = barrier.clone();
            std::thread::spawn(move || {
                b.wait();
                rename(&p, &s.id, &s.generation(), "shared").is_ok()
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .filter(|w| w.thread().id() != std::thread::current().id())
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let names = list(&p)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), 2);
}
#[test]
fn partial_discovery_never_replaces_a_still_live_attachment() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let stat = fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let s = record_launch(
        &p,
        "selected",
        "11111111-1111-4111-8111-111111111111",
        "Live",
        Some(Attachment {
            host: "herdr".into(),
            target: "herdr/other/w1:p1".into(),
            pid: std::process::id(),
            process_start: start,
        }),
    )
    .unwrap();
    let saved: crate::external::Instance = serde_json::from_value(serde_json::json!({"inactive":true,"account":"other","pane":"","tmux_session":"","pid":0,"cwd":"/tmp","thread_id":s.conversation_id,"rollout":"/tmp/fixture","home":"/tmp/home","binary":"codex","resume_options":[]})).unwrap();
    for items in [vec![], vec![saved]] {
        let after = reconcile(&p, &items).unwrap().remove(0);
        assert_eq!(after.generation(), s.generation());
        assert_eq!(after.account, "selected");
        assert_eq!(after.attachment, s.attachment);
    }
    transaction(&p, |c| {
        c.sessions
            .get_mut(&s.id)
            .unwrap()
            .attachment
            .as_mut()
            .unwrap()
            .process_start += 1;
        Ok(())
    })
    .unwrap();
    assert!(
        reconcile(&p, &[]).unwrap()[0].attachment.is_none(),
        "PID reuse remains a positive detachment"
    );
}
#[test]
fn disappeared_attachment_retains_owner_and_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(tmp.path().join("data"))).unwrap();
    let s = seed(&p);
    let result = reconcile(&p, &[]).unwrap();
    assert_eq!(result[0].account, "selected");
    assert_eq!(result[0].conversation_id, s.conversation_id);
    assert_eq!(result[0].id, s.id);
    assert!(result[0].attachment.is_none());
    assert!(resolve(&p, &s.id, &s.generation()).is_err());
}
