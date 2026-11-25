use super::super::*;
fn paths() -> (tempfile::TempDir, Paths) {
    let t = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(t.path().join("root"))).unwrap();
    (t, p)
}
fn found(id: &str) -> Discovered {
    Discovered {
        account: "main".into(),
        conversation_id: id.into(),
        label: "projects".into(),
        directory: Some("/work".into()),
    }
}
fn link(from: &str, to: &str) -> Continuation {
    Continuation {
        account: "main".into(),
        from: from.into(),
        to: to.into(),
    }
}
fn by_conversation(p: &Paths, id: &str) -> Session {
    list(p)
        .unwrap()
        .into_iter()
        .find(|s| s.conversation_id == id)
        .unwrap()
}
/// What a release without `aliases`/`previous_conversation_ids` does on any
/// write: both fields are dropped, and its discovery registers the continued
/// (old) conversations again under fresh ids and suffixed names.
fn old_release_rewrite(p: &Paths, reregister: &[&str]) -> Vec<String> {
    let file = p
        .root
        .join(crate::layout::SESSION_CATALOG)
        .join("catalog.json");
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    v.as_object_mut().unwrap().remove("aliases");
    for s in v["sessions"].as_object_mut().unwrap().values_mut() {
        s.as_object_mut()
            .unwrap()
            .remove("previous_conversation_ids");
    }
    let mut ids = vec![];
    for (n, conversation) in reregister.iter().enumerate() {
        let id = uuid::Uuid::new_v4().to_string();
        v["sessions"][&id] = serde_json::json!({"id":id,"name":format!("projects-{}", n + 10),
            "provider":"claude","account":"main","conversation_id":conversation,"revision":1,
            "attachment":null,"directory":"/work"});
        ids.push(id);
    }
    fs::write(&file, serde_json::to_vec(&v).unwrap()).unwrap();
    ids
}
fn merged(p: &Paths, id: &str, survivor: &str) {
    for e in [
        resolve(p, id, "any").unwrap_err().to_string(),
        rename(p, id, "any", "other-name").unwrap_err().to_string(),
        find(p, id).unwrap_err().to_string(),
        invalidate(p, id, "any").unwrap_err().to_string(),
    ] {
        assert!(
            e.starts_with("SESSION_MERGED") && e.contains(survivor),
            "{e}"
        );
    }
}

#[test]
fn two_chains_keep_their_entries_across_repeated_reconciles_and_old_release_rewrites() {
    let (_t, p) = paths();
    let claude = ProviderId::Claude;
    let codex = record_launch(&p, "codex-acct", "thread-x", "codex-one", None).unwrap();
    // Before the hand-offs: chains a1→a2 and b1→b2, every conversation listed.
    register_discovered(
        &p,
        claude,
        &[found("a1"), found("a2"), found("b1"), found("b2")],
    )
    .unwrap();
    let a1 = by_conversation(&p, "a1");
    let a1 = rename(&p, &a1.id, &a1.generation(), "sessionstuffs-claude").unwrap();
    let a2 = by_conversation(&p, "a2");
    let b1 = by_conversation(&p, "b1");
    let b2 = by_conversation(&p, "b2");
    let links = [link("a1", "a2"), link("b1", "b2")];
    let reversed = [link("b1", "b2"), link("a1", "a2")];
    // Discovery after the markers: the old conversations are not candidates,
    // the newest ones are (and are already owned).
    let candidates = [found("a2"), found("b2")];
    type Row = (String, String, String, Vec<String>);
    let mut seen: Option<Vec<Row>> = None;
    let mut reregistered = vec![];
    for round in 0..8 {
        if round == 3 || round == 5 {
            reregistered.extend(old_release_rewrite(&p, &["a1", "b1"]));
        }
        let order: &[Continuation] = if round % 2 == 0 { &links } else { &reversed };
        link_continuations(&p, claude, order).unwrap();
        register_discovered(&p, claude, &candidates).unwrap();
        let now: Vec<_> = list(&p)
            .unwrap()
            .into_iter()
            .filter(|s| s.provider == "claude")
            .map(|s| (s.id, s.name, s.conversation_id, s.previous_conversation_ids))
            .collect();
        assert_eq!(now.len(), 2, "round {round}: {now:?}");
        if let Some(seen) = &seen {
            assert_eq!(seen, &now, "round {round}");
        }
        seen = Some(now);
    }
    // The survivors are the pre-existing entries of the earliest conversations.
    let a = find(&p, &a1.id).unwrap();
    assert_eq!(a.name, "sessionstuffs-claude");
    assert_eq!(a.conversation_id, "a2");
    assert_eq!(a.previous_conversation_ids, ["a1"]);
    let b = find(&p, &b1.id).unwrap();
    assert_eq!(b.name, b1.name);
    assert_eq!(b.conversation_id, "b2");
    assert_eq!(b.previous_conversation_ids, ["b1"]);
    merged(&p, &a2.id, &a1.id);
    merged(&p, &b2.id, &b1.id);
    assert_eq!(reregistered.len(), 4);
    for (n, id) in reregistered.iter().enumerate() {
        merged(&p, id, if n % 2 == 0 { &a1.id } else { &b1.id });
    }
    // Restart: a fresh handle reads the same identity; an unchanged catalog is
    // not rewritten and an explicit repair has nothing left to restore.
    let file = p
        .root
        .join(crate::layout::SESSION_CATALOG)
        .join("catalog.json");
    let bytes = fs::read(&file).unwrap();
    let reloaded = Paths::new(Some(p.root.clone())).unwrap();
    assert_eq!(repair_identity(&reloaded).unwrap(), Healed::default());
    link_continuations(&reloaded, claude, &links).unwrap();
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(
        find(&reloaded, &a1.id).unwrap().name,
        "sessionstuffs-claude"
    );
    // Codex is untouched by Claude lineage and identity healing.
    let codex_now = find(&p, &codex.id).unwrap();
    assert_eq!(codex_now.generation(), codex.generation());
}

#[test]
fn explicit_repair_restores_lost_aliases_and_lineage_idempotently() {
    let (_t, p) = paths();
    let claude = ProviderId::Claude;
    register_discovered(&p, claude, &[found("a1"), found("a2")]).unwrap();
    let a1 = by_conversation(&p, "a1");
    let a2 = by_conversation(&p, "a2");
    link_continuations(&p, claude, &[link("a1", "a2")]).unwrap();
    old_release_rewrite(&p, &[]);
    let healed = repair_identity(&p).unwrap();
    assert_eq!(healed.restored_aliases, std::slice::from_ref(&a2.id));
    assert_eq!(healed.restored_lineage, std::slice::from_ref(&a1.id));
    assert_eq!(repair_identity(&p).unwrap(), Healed::default());
    merged(&p, &a2.id, &a1.id);
    assert_eq!(find(&p, &a1.id).unwrap().previous_conversation_ids, ["a1"]);
    // Without any identity record nothing is invented.
    let (_t2, q) = paths();
    register_discovered(&q, claude, &[found("x")]).unwrap();
    assert_eq!(repair_identity(&q).unwrap(), Healed::default());
    assert!(aliases(&q).unwrap().is_empty());
    assert!(
        find(&q, "91f47291-0000-4000-8000-000000000000")
            .unwrap_err()
            .to_string()
            .starts_with("SESSION_NOT_FOUND")
    );
}

#[test]
fn chains_converge_on_the_earliest_entry_in_any_link_order() {
    for order in [["a", "b"], ["b", "a"]] {
        let (_t, p) = paths();
        let claude = ProviderId::Claude;
        register_discovered(&p, claude, &[found("a"), found("b"), found("c")]).unwrap();
        let ea = by_conversation(&p, "a");
        let links: Vec<_> = order
            .iter()
            .map(|f| {
                if *f == "a" {
                    link("a", "b")
                } else {
                    link("b", "c")
                }
            })
            .collect();
        for _ in 0..3 {
            link_continuations(&p, claude, &links).unwrap();
        }
        let all = list(&p).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, ea.id);
        assert_eq!(all[0].name, ea.name);
        assert_eq!(all[0].conversation_id, "c");
        assert_eq!(all[0].previous_conversation_ids, ["a", "b"]);
        assert_eq!(aliases(&p).unwrap().len(), 2);
        assert!(aliases(&p).unwrap().values().all(|t| *t == ea.id));
    }
    // An entry that exists only for the newest conversation records the lineage.
    let (_t, p) = paths();
    register_discovered(&p, ProviderId::Claude, &[found("n")]).unwrap();
    let n = by_conversation(&p, "n");
    link_continuations(&p, ProviderId::Claude, &[link("o", "n")]).unwrap();
    let after = find(&p, &n.id).unwrap();
    assert_eq!(after.previous_conversation_ids, ["o"]);
    link_continuations(&p, ProviderId::Claude, &[link("o", "n")]).unwrap();
    assert_eq!(find(&p, &n.id).unwrap().generation(), after.generation());
}
