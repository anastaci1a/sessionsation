use super::*;
fn state_of(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')').next()?.trim().chars().next()
}
fn wait_for(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..250 {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}
#[test]
fn spawn_reaped_leaves_no_zombie_and_does_not_kill_a_slow_worker() {
    let dir = tempfile::tempdir().unwrap();
    let (pidfile, slow_done) = (dir.path().join("pid"), dir.path().join("slow"));
    let mut quick = Command::new("sh");
    detach(&mut quick, &dir.path().join("quick.log"))
        .args(["-c", "echo $$ > \"$1\"", "w"])
        .arg(&pidfile);
    quick.spawn_reaped().unwrap();
    let mut slow = Command::new("sh");
    detach(&mut slow, &dir.path().join("slow.log"))
        .args(["-c", "sleep 1; echo done > \"$1\"", "w"])
        .arg(&slow_done);
    slow.spawn_reaped().unwrap();
    assert!(wait_for(|| pidfile.exists()));
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Reaped means gone from /proc, never left as state 'Z'.
    assert!(wait_for(|| state_of(pid).is_none()), "worker is defunct");
    assert!(
        wait_for(|| slow_done.exists()),
        "dropping the handle must not kill the worker"
    );
}
#[test]
fn interrupted_preflight_releases_intent_but_possible_handoff_stays_uncertain() {
    for phase in ["admitted", "preparing"] {
        assert_eq!(interrupted_launch(phase).unwrap().0, "failed");
    }
    for phase in [
        "creating_terminal",
        "renaming",
        "dispatching",
        "starting_provider",
    ] {
        assert_eq!(interrupted_launch(phase).unwrap().0, "uncertain");
    }
    for phase in ["running", "attached", "uncertain", "transferring", "failed"] {
        assert!(interrupted_launch(phase).is_none());
    }
}
#[test]
fn durable_worker_has_its_own_process_group_and_no_inherited_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("identity");
    let mut command = Command::new("sh");
    command
        .env("CODEX_HOME", "synthetic")
        .env("OPENAI_API_KEY", "synthetic");
    detach(&mut command, &dir.path().join("w.log"))
        .args([
            "-c",
            "test -z \"$CODEX_HOME$OPENAI_API_KEY\" && echo $$ > \"$1\"; sleep 1",
            "worker",
        ])
        .arg(&output);
    let mut child = command.spawn().unwrap();
    let group = unsafe { libc::getpgid(child.id() as i32) };
    assert_eq!(group, child.id() as i32);
    assert_ne!(group, unsafe { libc::getpgrp() });
    assert!(child.wait().unwrap().success());
    assert_eq!(
        std::fs::read_to_string(output).unwrap().trim(),
        child.id().to_string()
    );
}

/// Spawn through `spawn` a worker that outlives a held, inheritable lock; it must not hold it.
fn assert_worker_does_not_hold_lock(spawn: impl Fn(&mut Command) -> std::io::Result<Worker>) {
    use fs2::FileExt;
    use std::os::fd::AsRawFd;
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("lock");
    let pidfile = dir.path().join("pid");
    let held = std::fs::File::create(&lock_path).unwrap();
    // flock(1) hands its lock fd to the command: no CLOEXEC.
    unsafe { libc::fcntl(held.as_raw_fd(), libc::F_SETFD, 0) };
    held.lock_exclusive().unwrap();
    let mut worker = Command::new("sh");
    worker
        .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "w"])
        .arg(&pidfile);
    spawn(&mut worker).unwrap();
    assert!(wait_for(
        || std::fs::read_to_string(&pidfile).is_ok_and(|s| s.trim().parse::<u32>().is_ok())
    ));
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait_for(|| std::fs::read_link(format!("/proc/{pid}/exe"))
        .is_ok_and(
            |p| p.ends_with("sleep") || p.to_string_lossy().contains("busybox")
        )));
    let leaked = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.unwrap().path()).ok())
        .any(|l| l == lock_path);
    // The client finishes while the worker keeps running.
    held.unlock().unwrap();
    let other = std::fs::File::open(&lock_path).unwrap();
    let free = other.try_lock_exclusive().is_ok();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    assert!(!leaked, "the worker inherited the client's lock fd");
    assert!(free, "the running worker still holds the client's lock");
}
#[test]
fn detach_workers_do_not_inherit_a_clients_lock() {
    assert_worker_does_not_hold_lock(|c| detach(c, Path::new("/dev/null")).spawn_reaped());
}
#[test]
fn detach_session_workers_do_not_inherit_a_clients_lock() {
    assert_worker_does_not_hold_lock(|c| detach_session(c, Path::new("/dev/null")).spawn_reaped());
}
#[test]
fn handoff_style_workers_do_not_inherit_a_clients_lock() {
    assert_worker_does_not_hold_lock(|c| {
        no_inherited_fds(c)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn_reaped()
    });
}
/// Contract 1.68.1: a worker that dies before doing anything leaves its reason in its log,
/// and the log restarts instead of growing without bound.
#[test]
fn a_worker_that_dies_early_leaves_its_stderr_in_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("op.worker.log");
    for _ in 0..2 {
        detach(&mut Command::new("sh"), &log)
            .args(["-c", "echo 'worker refused: LAYOUT_INVALID' >&2; exit 1"])
            .spawn_reaped()
            .unwrap();
    }
    assert!(wait_for(|| std::fs::read_to_string(&log).is_ok_and(|s| s
        .matches("worker refused: LAYOUT_INVALID")
        .count()
        == 2)));
    let text = std::fs::read_to_string(&log).unwrap();
    assert_eq!(text.matches(" started by ").count(), 2, "{text}");
    std::fs::write(&log, vec![b'x'; 70 * 1024]).unwrap();
    detach(&mut Command::new("sh"), &log)
        .args(["-c", "echo again >&2"])
        .spawn_reaped()
        .unwrap();
    assert!(wait_for(
        || std::fs::read_to_string(&log).is_ok_and(|s| s.ends_with("again\n"))
    ));
    assert!(std::fs::metadata(&log).unwrap().len() < 1024);
    let mut session = Command::new("sh");
    detach_session(&mut session, &log).args(["-c", "echo session >&2"]);
    session.spawn_reaped().unwrap();
    assert!(wait_for(
        || std::fs::read_to_string(&log).is_ok_and(|s| s.ends_with("session\n"))
    ));
}
/// The identity `spawn_reaped` returns names the running worker until it exits.
#[test]
fn spawn_reaped_returns_the_workers_exact_identity() {
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("go");
    let worker = detach(&mut Command::new("sh"), &dir.path().join("w.log"))
        .args(["-c", "while [ ! -e \"$1\" ]; do sleep 0.05; done", "w"])
        .arg(&gate)
        .spawn_reaped()
        .unwrap();
    assert_eq!(
        crate::proc::identity::start_time(worker.pid).unwrap(),
        worker.start
    );
    assert!(!crate::proc::identity::gone(worker.pid, worker.start));
    std::fs::write(&gate, "").unwrap();
    assert!(wait_for(|| crate::proc::identity::gone(
        worker.pid,
        worker.start
    )));
}
