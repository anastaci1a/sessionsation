use super::super::*;
fn paths() -> (tempfile::TempDir, Paths) {
    let t = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(t.path().join("root"))).unwrap();
    (t, p)
}
fn found(account: &str, id: &str, label: &str) -> Discovered {
    Discovered {
        account: account.into(),
        conversation_id: id.into(),
        label: label.into(),
        directory: Some("/work".into()),
    }
}
#[test]
fn continuation_inherits_the_entry_merges_the_duplicate_and_never_reappears() {
    let (_t, p) = paths();
    let claude = ProviderId::Claude;
    let all = register_discovered(
        &p,
        claude,
        &[found("main", "a", "work"), found("main", "b", "work")],
    )
    .unwrap();
    assert_eq!(all.len(), 2);
    let old = all.iter().find(|s| s.conversation_id == "a").unwrap();
    let old = rename(&p, &old.id, &old.generation(), "sessionstuffs").unwrap();
    let dup = all
        .iter()
        .find(|s| s.conversation_id == "b")
        .unwrap()
        .clone();
    // Cross-account evidence is never applied.
    let wrong = Continuation {
        account: "other".into(),
        from: "a".into(),
        to: "b".into(),
    };
    assert!(link_continuations(&p, claude, &[wrong]).unwrap().is_empty());
    assert_eq!(list(&p).unwrap().len(), 2);
    let link = Continuation {
        account: "main".into(),
        from: "a".into(),
        to: "b".into(),
    };
    let merged = link_continuations(&p, claude, &[link.clone(), link.clone()]).unwrap();
    assert_eq!(merged, vec![(dup.id.clone(), old.id.clone())]);
    let now = list(&p).unwrap();
    assert_eq!(now.len(), 1, "one lineage, one entry");
    assert_eq!(now[0].id, old.id);
    assert_eq!(now[0].name, "sessionstuffs");
    assert_eq!(now[0].account, "main");
    assert_eq!(now[0].conversation_id, "b");
    assert_eq!(now[0].previous_conversation_ids, ["a"]);
    let e = resolve(&p, &dup.id, &dup.generation())
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("SESSION_MERGED") && e.contains(&old.id),
        "{e}"
    );
    // Rediscovery of the continued transcript never recreates an entry.
    let again = register_discovered(
        &p,
        claude,
        &[found("main", "a", "x"), found("main", "b", "x")],
    )
    .unwrap();
    assert_eq!(again.len(), 1);
    // Chains follow to the newest continuation.
    register_discovered(&p, claude, &[found("main", "c", "x")]).unwrap();
    link_continuations(
        &p,
        claude,
        &[
            link,
            Continuation {
                account: "main".into(),
                from: "b".into(),
                to: "c".into(),
            },
        ],
    )
    .unwrap();
    let now = list(&p).unwrap();
    assert_eq!(now.len(), 1);
    assert_eq!(now[0].conversation_id, "c");
    assert_eq!(now[0].previous_conversation_ids, ["a", "b"]);
}
#[test]
fn codex_process_switching_threads_is_attached_to_exactly_one_entry() {
    let (_t, p) = paths();
    let pid = std::process::id();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start: u64 = stat
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    let attachment = Attachment {
        host: "tmux".into(),
        target: "%7".into(),
        pid,
        process_start: start,
    };
    let a = record_launch(&p, "acct", "thread-a", "first", Some(attachment.clone())).unwrap();
    let b = record_launch(&p, "acct", "thread-b", "second", None).unwrap();
    // The same TUI process now verifiably holds thread-b.
    let i: crate::external::Instance = serde_json::from_value(serde_json::json!({"pane":"%7","tmux_session":"t","pid":pid,"start_time":start,"cwd":"/tmp","thread_id":"thread-b","rollout":"/tmp/r","home":"/tmp/h","binary":"codex","resume_options":[],"account":"acct"})).unwrap();
    let all = reconcile(&p, &[i]).unwrap();
    assert_eq!(all.len(), 2, "no duplicate entry is created");
    let a2 = all.iter().find(|s| s.id == a.id).unwrap();
    let b2 = all.iter().find(|s| s.id == b.id).unwrap();
    assert!(a2.attachment.is_none(), "the entry it left is released");
    assert_eq!(a2.name, "first");
    assert_eq!(b2.attachment.as_ref(), Some(&attachment));
    assert_eq!(b2.name, "second");
    assert_eq!(
        all.iter()
            .filter(|s| s.attachment.as_ref().is_some_and(|x| x.pid == pid))
            .count(),
        1
    );
}
