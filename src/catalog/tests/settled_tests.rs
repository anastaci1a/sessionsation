use super::super::*;
use super::SETTLED_HITS;

fn root() -> (tempfile::TempDir, Paths) {
    crate::file_cache::trust_fresh_stamps(true);
    let t = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(t.path().join("root"))).unwrap();
    (t, p)
}
fn seed(p: &Paths, n: usize) -> Vec<Session> {
    let found: Vec<Discovered> = (0..n)
        .map(|i| Discovered {
            account: "a".into(),
            conversation_id: format!("conv-{i}"),
            label: format!("label-{i}"),
            directory: Some("/work".into()),
        })
        .collect();
    register_discovered(p, ProviderId::Claude, &found).unwrap()
}
fn files(p: &Paths) -> Vec<Vec<u8>> {
    let dir = p.root.join(crate::layout::SESSION_CATALOG);
    ["catalog.json", "identity.json", "spaces.json"]
        .iter()
        .map(|f| fs::read(dir.join(f)).unwrap_or_default())
        .collect()
}
fn hits() -> usize {
    SETTLED_HITS.with(|h| h.get())
}
/// Forget the in-process state so the next call reads the files (the reference answer).
fn forget(p: &Paths) {
    SETTLED
        .lock()
        .unwrap()
        .remove(&p.root.join(crate::layout::SESSION_CATALOG));
}

#[test]
fn reads_of_a_settled_catalog_match_a_fresh_read_and_touch_nothing() {
    let (_t, p) = root();
    let all = seed(&p, 12);
    let first = list(&p).unwrap(); // full transaction: nothing to write, state remembered
    let before = files(&p);
    let h = hits();
    let again = list(&p).unwrap();
    assert!(
        hits() > h,
        "the second read is served from the settled state"
    );
    assert_eq!(first, again);
    for s in &all {
        let expected = first.iter().find(|x| x.id == s.id).unwrap();
        assert_eq!(&find(&p, &s.id).unwrap(), expected);
        assert_eq!(resolve(&p, &s.id, &s.generation()).unwrap().id, s.id);
        assert!(
            resolve(&p, &s.id, "stale")
                .unwrap_err()
                .to_string()
                .contains("STALE_TARGET")
        );
    }
    assert!(
        find(&p, "missing")
            .unwrap_err()
            .to_string()
            .contains("SESSION_NOT_FOUND")
    );
    assert_eq!(aliases(&p).unwrap().len(), 0);
    assert_eq!(files(&p), before, "reads write nothing");
    forget(&p);
    assert_eq!(
        list(&p).unwrap(),
        again,
        "same as a read that parses the files"
    );
}
#[test]
fn a_write_under_the_lock_invalidates_the_settled_state() {
    let (_t, p) = root();
    let all = seed(&p, 3);
    list(&p).unwrap();
    list(&p).unwrap();
    let renamed = rename(&p, &all[0].id, &all[0].generation(), "renamed").unwrap();
    assert_eq!(find(&p, &all[0].id).unwrap().name, "renamed");
    assert_eq!(
        find(&p, &all[0].id).unwrap().generation(),
        renamed.generation()
    );
    // A mutation from the settled state persists and is what the next process reads.
    list(&p).unwrap();
    let h = hits();
    transaction(&p, |c| {
        c.sessions.get_mut(&all[1].id).unwrap().revision += 10;
        Ok(())
    })
    .unwrap();
    assert!(hits() > h, "the mutation started from the settled state");
    forget(&p);
    assert_eq!(find(&p, &all[1].id).unwrap().revision, all[1].revision + 10);
    // A closure that changes nothing, or fails, leaves the files alone.
    list(&p).unwrap();
    let before = files(&p);
    transaction(&p, |c| Ok(c.sessions.len())).unwrap();
    assert!(
        transaction(&p, |c| -> Result<()> {
            c.sessions.clear();
            anyhow::bail!("no")
        })
        .is_err()
    );
    assert_eq!(files(&p), before);
    assert_eq!(list(&p).unwrap().len(), 3);
}

#[test]
fn a_rewrite_of_any_catalog_file_is_seen_and_healed_not_served_stale() {
    let (_t, p) = root();
    let all = seed(&p, 2);
    list(&p).unwrap();
    list(&p).unwrap();
    // An older release rewrites identity.json with a merged id (a different size).
    let dir = p.root.join(crate::layout::SESSION_CATALOG);
    let target = all[1].id.clone();
    fs::write(
        dir.join("identity.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "aliases": {"11111111-1111-4111-8111-111111111111": target},
            "lineage": {}
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        aliases(&p)
            .unwrap()
            .get("11111111-1111-4111-8111-111111111111"),
        Some(&target),
        "the identity record was read again and its alias restored"
    );
    // catalog.json replaced by an older release's shape (an entry lost): the entry is
    // never recreated, but the file is read again, not served from memory.
    list(&p).unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("catalog.json")).unwrap()).unwrap();
    value["sessions"]
        .as_object_mut()
        .unwrap()
        .remove(&all[0].id);
    fs::write(
        dir.join("catalog.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert_eq!(list(&p).unwrap().len(), 1);
}
