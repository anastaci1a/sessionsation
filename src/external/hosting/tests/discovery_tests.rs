use super::super::*;
#[test]
fn strict_launch_watch_detects_exit_after_busy() {
    assert_eq!(watch_pane(false, true), PaneWatch::Waiting);
    assert_eq!(watch_pane(false, false), PaneWatch::Waiting);
    assert_eq!(watch_pane(true, false), PaneWatch::Waiting);
    assert_eq!(watch_pane(true, true), PaneWatch::Exited);
    assert!(unconfirmed_error("p", true).starts_with("RESUME_LAUNCH_FAILED"));
    assert!(unconfirmed_error("p", false).starts_with("RESUME_LAUNCH_UNCONFIRMED: "));
    assert!(unconfirmed_error("pane-1", false).contains("pane-1"));
}
#[test]
fn live_process_socket_requires_the_same_process() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .env("HERDR_SOCKET_PATH", "/tmp/herdr-test/herdr.sock")
        .spawn()
        .unwrap();
    let pid = child.id();
    // Wait until the child has exec'd (its environ is then the new image's).
    for _ in 0..200 {
        if fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c.starts_with(b"sleep\0")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let start = process_info(pid).unwrap().start_time;
    assert_eq!(
        live_process_socket(pid, start),
        Some(PathBuf::from("/tmp/herdr-test/herdr.sock"))
    );
    // A different start time is a different (reused-PID) process.
    assert_eq!(live_process_socket(pid, start + 1), None);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(live_process_socket(pid, start), None);
}
#[test]
fn stale_bindings_never_contact_a_host_and_remain_recoverable() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    let pid = std::process::id();
    let start = process_info(pid).unwrap().start_time;
    for (index, (pid, start_time)) in [(u32::MAX, 0), (pid, start + 1)].into_iter().enumerate() {
        bind(
            &paths,
            &format!("herdr/stopped/w1:p{}", index + 1),
            Binding {
                pid,
                start_time,
                home: dir.path().join("home"),
                thread: "11111111-1111-4111-8111-111111111111".into(),
            },
        )
        .unwrap();
    }
    let files: Vec<_> = fs::read_dir(paths.root.join(crate::layout::HERDR_PANES))
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            let bytes = fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect();
    let found = instances_with_state(&paths, |_, _| {
        panic!("stale process must not trigger HERDR socket I/O")
    })
    .unwrap();
    assert!(found.is_empty());
    for (path, bytes) in files {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}
#[test]
fn bindings_retain_socket_across_reader_environments_and_process_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(Some(tmp.path().join("state"))).unwrap();
    let socket = PathBuf::from("/isolated/herdr.sock");
    let mut child = Command::new("sleep")
        .arg("30")
        .env("HERDR_SOCKET_PATH", &socket)
        .spawn()
        .unwrap();
    // Wait until the child has exec'd; before that /proc/<pid>/environ is still
    // the forked parent's environment, which lacks HERDR_SOCKET_PATH.
    for _ in 0..1000 {
        if process_socket(child.id()).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(process_socket(child.id()), Some(socket.clone()));
    let pane = "herdr/named/w1:p1";
    bind(
        &paths,
        pane,
        Binding {
            pid: child.id(),
            start_time: process_info(child.id()).unwrap().start_time,
            home: tmp.path().join("home"),
            thread: "11111111-1111-4111-8111-111111111111".into(),
        },
    )
    .unwrap();
    assert_eq!(socket_for(&paths, pane).unwrap(), Some(socket.clone()));
    let mut calls = 0;
    instances_with_state(&paths, |key, locator| {
        assert_eq!(key, pane);
        assert_eq!(locator, Some(socket.as_path()));
        calls += 1;
        Ok((true, child.id()))
    })
    .unwrap();
    assert_eq!(calls, 1);
    let bytes = fs::read(binding_file(&paths, pane)).unwrap();
    let mut legacy: Value = serde_json::from_slice(&bytes).unwrap();
    legacy.as_object_mut().unwrap().remove("socket");
    fs::write(
        binding_file(&paths, pane),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    assert_eq!(
        socket_for(&paths, pane).unwrap(),
        Some(socket.clone()),
        "legacy live bindings derive their own process socket, not the reader's"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(binding_file(&paths, pane)).unwrap()).unwrap(),
        legacy,
        "observation does not migrate legacy binding files"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    private_dir(&paths.root.join(crate::layout::RESUMPTIONS)).unwrap();
    fs::write(paths.root.join(crate::layout::RESUMPTIONS).join("fixture.json"), serde_json::to_vec(&json!({"request":{"host":{"session":"named","socket":socket}},"pane_id":"w1:p1","conversation_id":"11111111-1111-4111-8111-111111111111","created_at":1})).unwrap()).unwrap();
    assert_eq!(
        socket_for(&paths, pane).unwrap(),
        Some(socket.clone()),
        "legacy stopped bindings use their own durable launch locator"
    );
    fs::write(binding_file(&paths, pane), bytes).unwrap();
    assert_eq!(
        socket_for(&paths, pane).unwrap(),
        Some(socket),
        "retained socket is available for safe relaunch after process exit"
    );
}
/// A binding recorded before `layout migrate` names the legacy home; the
/// live process now reports the new home (the legacy path is a symlink).
/// The session must stay discoverable, with the canonical home.
#[test]
fn legacy_binding_home_still_matches_the_live_process_after_migration() {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    let paths = Paths::new(Some(root.join("state"))).unwrap();
    let new = root.join("state/accounts/codex/gate1");
    let thread = "22222222-2222-4222-8222-222222222222";
    let rollout = new.join(format!(
        "sessions/2026/10/29/rollout-2026-10-29T09-20-33-{thread}.jsonl"
    ));
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(
        &rollout,
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{thread}\"}}}}\n"),
    )
    .unwrap();
    let legacy = root.join("state/accounts/gate1");
    std::os::unix::fs::symlink("codex/gate1", &legacy).unwrap();
    let script = root.join("codex");
    fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    let mut child = Command::new("sh")
        .arg(&script)
        .args(["resume", thread])
        .env("CODEX_HOME", &new)
        .spawn()
        .unwrap();
    let pid = child.id();
    for _ in 0..1000 {
        if process_home(pid).is_ok_and(|h| h == new) && resumed_thread(pid).ok().flatten().is_some()
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let pane = "herdr/named/w1:p1";
    bind(
        &paths,
        pane,
        Binding {
            pid,
            start_time: process_info(pid).unwrap().start_time,
            home: legacy,
            thread: thread.into(),
        },
    )
    .unwrap();
    let found = instances_with_state(&paths, |_, _| Ok((false, pid))).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].home, new);
    assert_eq!(found[0].thread_id, thread);
}
#[test]
fn live_pid_still_requires_current_host_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        root: dir.path().join("state"),
        runtime: dir.path().join("runtime"),
    };
    let pid = std::process::id();
    bind(
        &paths,
        "herdr/named/w1:p1",
        Binding {
            pid,
            start_time: process_info(pid).unwrap().start_time,
            home: dir.path().join("home"),
            thread: "11111111-1111-4111-8111-111111111111".into(),
        },
    )
    .unwrap();
    let mut calls = 0;
    let found = instances_with_state(&paths, |target, _| {
        assert_eq!(target, "herdr/named/w1:p1");
        calls += 1;
        Ok((true, pid))
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert!(found.is_empty());
}
