use super::*;
use fs2::FileExt;
use serde_json::json;
#[test]
fn failed_handoff_keeps_reservation_until_recovery_worker_exits() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let thread = uuid::Uuid::new_v4().to_string();
    private_dir(&p.root.join(crate::layout::HANDOFF_JOBS)).unwrap();
    fs::write(
        p.root.join(crate::layout::HANDOFF_JOBS).join("move.json"),
        serde_json::to_vec(&json!({"id":"move","source":{"thread_id":thread},"phase":"failed"}))
            .unwrap(),
    )
    .unwrap();
    let lock = lock_file(
        &p.root
            .join(crate::layout::HANDOFF_JOBS)
            .join("move.worker-lock"),
    )
    .unwrap();
    lock.lock_exclusive().unwrap();
    assert_eq!(pending(&p, &thread, None).unwrap().as_deref(), Some("move"));
    assert!(admit(&p, &thread, None).is_err());
    assert!(admit(&p, &thread, Some("move")).is_ok());
    drop(lock);
    assert!(admit(&p, &thread, None).is_ok());
}
#[test]
fn competing_operations_reject_pending_and_uncertain_resumes_but_allow_original_handoff() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let thread = uuid::Uuid::new_v4().to_string();
    private_dir(&p.root.join(crate::layout::RESUMPTIONS)).unwrap();
    for phase in [
        "admitted",
        "preparing",
        "creating_terminal",
        "dispatching",
        "transferring",
        "uncertain",
    ] {
        fs::write(p.root.join(crate::layout::RESUMPTIONS).join("original.json"), serde_json::to_vec(&json!({"conversation_id":thread,"request":{"request_id":"original"},"phase":phase})).unwrap()).unwrap();
        assert!(admit(&p, &thread, Some("competing")).is_err(), "{phase}");
        let original = admit(&p, &thread, Some("original")).unwrap();
        assert!(
            admit(&p, &thread, Some("original")).is_err(),
            "simultaneous admission must lock"
        );
        drop(original);
    }
    fs::write(p.root.join(crate::layout::RESUMPTIONS).join("original.json"), serde_json::to_vec(&json!({"conversation_id":thread,"request":{"request_id":"original"},"phase":"running"})).unwrap()).unwrap();
    assert!(admit(&p, &thread, None).is_ok());
    private_dir(&p.root.join(crate::layout::CONVERSATION_OPERATIONS)).unwrap();
    fs::write(
        p.root
            .join(crate::layout::CONVERSATION_OPERATIONS)
            .join("archive.json"),
        serde_json::to_vec(
            &json!({"id":"archive","session":{"conversation_id":thread},"phase":"queued"}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(admit(&p, &thread, None).is_err());
    private_dir(&p.root.join(crate::layout::HANDOFF_JOBS)).unwrap();
    fs::write(
        p.root.join(crate::layout::HANDOFF_JOBS).join("move.json"),
        serde_json::to_vec(
            &json!({"id":"move","source":{"thread_id":thread},"phase":"launching_target"}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(ensure_no_handoff(&p, &thread, None).is_err());
    assert!(ensure_no_handoff(&p, &thread, Some("move")).is_ok());
}
/// A journal entry parsed once and served from memory answers like a fresh read, and a
/// rewrite, removal or corruption of the file is seen on the next pass.
#[test]
fn cached_journal_entries_follow_the_files() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let (a, b) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    let dir = p.root.join(crate::layout::CONVERSATION_OPERATIONS);
    private_dir(&dir).unwrap();
    let put = |name: &str, id: &str, thread: &str, phase: &str| {
        fs::write(
            dir.join(name),
            serde_json::to_vec(
                &json!({"id":id,"session":{"conversation_id":thread},"phase":phase,"noise":[1,2,3]}),
            )
            .unwrap(),
        )
        .unwrap();
    };
    let answer = |thread: &str| {
        let cached = (
            pending_detail(&p, thread, None),
            PendingIndex::scan(&p).get(thread),
        );
        crate::file_cache::trust_fresh_stamps(false);
        let fresh = (
            pending_detail(&p, thread, None),
            PendingIndex::scan(&p).get(thread),
        );
        crate::file_cache::trust_fresh_stamps(true);
        let text = |r: Result<Option<Pending>>| r.map_err(|e| format!("{e:#}"));
        assert_eq!(text(cached.0), text(fresh.0), "{thread}");
        assert_eq!(text(cached.1), text(fresh.1), "{thread}");
        pending_detail(&p, thread, None).map_err(|e| format!("{e:#}"))
    };
    crate::file_cache::trust_fresh_stamps(true);
    put("x.json", "x", &a, "queued");
    put("y.json", "y", &b, "complete");
    for _ in 0..2 {
        assert_eq!(answer(&a).unwrap().unwrap().id, "x");
        assert!(answer(&b).unwrap().is_none());
    }
    // Rewritten in place (different size): no longer blocks.
    put("x.json", "x", &a, "cancelled");
    assert!(answer(&a).unwrap().is_none());
    // Another operation appears for the same conversation.
    put("z.json", "z", &a, "queued");
    assert_eq!(answer(&a).unwrap().unwrap().id, "z");
    // Removed.
    fs::remove_file(dir.join("z.json")).unwrap();
    assert!(answer(&a).unwrap().is_none());
    // Corrupted: blocks with the parse error, like a fresh read.
    fs::write(dir.join("y.json"), b"{").unwrap();
    assert!(answer(&b).is_err());
    crate::file_cache::trust_fresh_stamps(false);
}
/// One pass over the journals answers every conversation exactly as a scan per
/// conversation does, including which ones an unreadable journal blocks.
#[test]
fn the_batch_index_answers_like_a_scan_per_conversation() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    let ids: Vec<String> = (0..6).map(|_| uuid::Uuid::new_v4().to_string()).collect();
    let write = |dir: &str, name: &str, v: Value| {
        private_dir(&p.root.join(dir)).unwrap();
        fs::write(p.root.join(dir).join(name), serde_json::to_vec(&v).unwrap()).unwrap();
    };
    let compare = |p: &Paths| {
        let index = PendingIndex::scan(p);
        for id in ids.iter().map(String::as_str).chain(["unknown-thread"]) {
            let one = pending_detail(p, id, None).map_err(|e| format!("{e:#}"));
            let batch = index.get(id).map_err(|e| format!("{e:#}"));
            assert_eq!(one, batch, "{id}");
        }
    };
    compare(&p);
    write(
        crate::layout::RESUMPTIONS,
        "a.json",
        json!({"conversation_id":ids[0],"request":{"request_id":"a"},"phase":"uncertain"}),
    );
    write(
        crate::layout::CONVERSATION_OPERATIONS,
        "b.json",
        json!({"id":"b","session":{"conversation_id":ids[1]},"phase":"queued"}),
    );
    write(
        crate::layout::CONVERSATION_OPERATIONS,
        "c.json",
        json!({"id":"c","session":{"conversation_id":ids[1]},"phase":"queued"}),
    );
    write(
        crate::layout::CONVERSATION_OPERATIONS,
        "d.json",
        json!({"id":"d","session":{"conversation_id":ids[2]},"phase":"complete"}),
    );
    write(
        crate::layout::HANDOFF_JOBS,
        "e.json",
        json!({"id":"e","source":{"thread_id":ids[3]},"phase":"failed"}),
    );
    write(
        crate::layout::HANDOFF_JOBS,
        "f.json",
        json!({"source":{"thread_id":ids[4]},"phase":"failed"}),
    );
    write(
        crate::layout::SPACE_MOVES,
        "g.json",
        json!({"conversation_id":ids[5],"phase":"moving"}),
    );
    compare(&p);
    assert_eq!(
        PendingIndex::scan(&p).get(&ids[1]).unwrap().unwrap().id,
        "b"
    );
    // An unreadable journal blocks only what no earlier entry settled.
    fs::write(
        p.root
            .join(crate::layout::CONVERSATION_OPERATIONS)
            .join("bad.json"),
        b"{",
    )
    .unwrap();
    compare(&p);
    let index = PendingIndex::scan(&p);
    assert!(index.get(&ids[0]).unwrap().is_some());
    assert!(index.get(&ids[2]).is_err());
    assert!(index.get("unknown-thread").is_err());
}
