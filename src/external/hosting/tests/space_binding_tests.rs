use super::super::*;
/// A binding whose process is gone is skipped without contacting HERDR, and remembered
/// as gone; a binding of a live process still reaches the host check. Cached records follow
/// a rewrite of the file.
#[test]
fn gone_bindings_are_skipped_for_good_and_live_ones_still_reach_the_host() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    crate::file_cache::trust_fresh_stamps(true);
    private_dir(&p.root.join(crate::layout::HERDR_PANES)).unwrap();
    // This process is the live one; a pid that cannot exist is the exited one.
    let me = std::process::id();
    let start = process_info(me).unwrap().start_time;
    let nobody = u32::MAX - 1;
    let bind_to = |pane: &str, pid: u32, start_time: u64| {
        atomic_write(
            &binding_file(&p, pane),
            &serde_json::to_vec(&HerdrBinding {
                target: pane.into(),
                socket: Some(p.root.join("s.sock")),
                binding: Binding {
                    pid,
                    start_time,
                    home: p.root.join("home"),
                    thread: uuid::Uuid::new_v4().to_string(),
                },
            })
            .unwrap(),
        )
        .unwrap();
    };
    let (live, gone) = ("herdr/fixture/w1:p1", "herdr/fixture/w1:p2");
    bind_to(live, me, start);
    bind_to(gone, me, start + 1); // the pid now belongs to another process
    bind_to("herdr/fixture/w1:p3", nobody, 7);
    let mut asked = vec![];
    let none = instances_with_state(&p, |pane, _| {
        asked.push(pane.to_owned());
        anyhow::bail!("host unavailable")
    })
    .unwrap();
    assert!(none.is_empty());
    assert_eq!(
        asked,
        [live],
        "only the live process reaches the host check"
    );
    assert!(GONE.contains(me, start + 1));
    assert!(GONE.contains(nobody, 7));
    assert!(!GONE.contains(me, start));
    // Same answers from the cache, and a rewritten record is read again.
    let mut asked = vec![];
    instances_with_state(&p, |pane, _| {
        asked.push(pane.to_owned());
        anyhow::bail!("host unavailable")
    })
    .unwrap();
    assert_eq!(asked, [live]);
    // The live pane's record is rewritten: the next pass reads the new content.
    bind_to(live, me, start + 2);
    let mut asked = vec![];
    instances_with_state(&p, |pane, _| {
        asked.push(pane.to_owned());
        anyhow::bail!("host unavailable")
    })
    .unwrap();
    assert!(
        asked.is_empty(),
        "the rewritten binding names a gone identity"
    );
    crate::file_cache::trust_fresh_stamps(false);
}
#[test]
fn binding_write_rename_gap_recovers_and_cannot_replace_another_process() {
    let d = tempfile::tempdir().unwrap();
    let p = Paths::new(Some(d.path().join("state"))).unwrap();
    private_dir(&p.root.join(crate::layout::HERDR_PANES)).unwrap();
    let old = "herdr/fixture/w1:p1";
    let new = "herdr/fixture/w2:p2";
    let socket = p.root.join("owned.sock");
    let binding = Binding {
        pid: 11,
        start_time: 22,
        home: p.root.join("home"),
        thread: uuid::Uuid::new_v4().to_string(),
    };
    // Simulate interruption after publishing the new target in the old file.
    atomic_write(
        &binding_file(&p, old),
        &serde_json::to_vec(&HerdrBinding {
            target: new.into(),
            binding: binding.clone(),
            socket: Some(socket.clone()),
        })
        .unwrap(),
    )
    .unwrap();
    rebind(&p, old, new, &socket, &binding).unwrap();
    assert!(!binding_file(&p, old).exists());
    rebind(&p, old, new, &socket, &binding).unwrap();
    let changed = Binding { pid: 99, ..binding };
    assert!(rebind(&p, old, new, &socket, &changed).is_err());
}
