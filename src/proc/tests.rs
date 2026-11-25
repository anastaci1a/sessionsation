use super::identity::*;
use std::io;
use std::process::{Child, Command};

/// A child this test spawned; killed and reaped on drop (only its own pid).
struct Spawned(Child);
impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Spawned {
    fn sleeper() -> (Self, u32, u64) {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let start = start_time(pid).unwrap();
        (Self(child), pid, start)
    }
    fn running(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}
fn eventually(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..250 {
        if f() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

#[test]
fn parses_a_command_name_with_spaces_and_parens() {
    let line = "4242 (a) b (c)) S 17 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1 2";
    let s = parse(4242, line).unwrap();
    assert_eq!(
        s,
        Stat {
            pid: 4242,
            parent: 17,
            state: 'S',
            start_time: 987654
        }
    );
    assert_eq!(
        parse(1, "1 (x").unwrap_err().to_string(),
        "invalid process stat"
    );
    assert_eq!(
        parse(1, "1 (x) S 0 1").unwrap_err().to_string(),
        "short process stat"
    );
}

#[test]
fn the_own_process_is_alive_and_a_reused_pid_is_not() {
    let me = std::process::id();
    let start = start_time(me).unwrap();
    assert_eq!(proc_start(me).map(|(_, s)| s), Some(start));
    assert!(alive(me, start) && !gone(me, start) && same(me, start));
    // Pid reuse: the same pid with another start time is another process.
    assert!(!alive(me, start + 1) && gone(me, start + 1) && !same(me, start + 1));
}

#[test]
fn a_dead_pid_is_gone_and_its_stat_is_not_found() {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    let start = start_time(pid).unwrap();
    // Unreaped, the exited child is a zombie: not alive, but the pid still names it.
    assert!(eventually(|| proc_start(pid).is_some_and(|(s, _)| s == 'Z')));
    assert!(!alive(pid, start) && gone(pid, start) && same(pid, start));
    child.wait().unwrap();
    assert!(gone(pid, start) && !same(pid, start));
    assert_eq!(proc_start(pid), None);
    let error = stat(pid).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::NotFound)
    );
}

#[test]
fn a_mismatched_start_time_is_never_signalled() {
    let (mut child, pid, start) = Spawned::sleeper();
    assert_eq!(
        signal_exact(pid, start + 1, libc::SIGTERM).unwrap(),
        Signalled::Gone
    );
    assert_eq!(
        signal_exact(pid, start.wrapping_sub(1), libc::SIGKILL).unwrap(),
        Signalled::Gone
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(child.running(), "a mismatched identity was signalled");
    assert!(alive(pid, start));
    assert_eq!(
        signal_exact(pid, start, libc::SIGTERM).unwrap(),
        Signalled::Sent
    );
    assert!(eventually(|| !child.running()));
    // The exact process is gone now (reaped): nothing to signal, no error.
    assert_eq!(
        signal_exact(pid, start, libc::SIGTERM).unwrap(),
        Signalled::Gone
    );
}

#[test]
fn a_pidfd_keeps_naming_its_process() {
    let (mut child, pid, _) = Spawned::sleeper();
    let fd = Pidfd::open(pid).unwrap();
    fd.signal(0).unwrap();
    fd.signal(libc::SIGTERM).unwrap();
    assert!(eventually(|| !child.running()));
    // Exited and reaped: the handle still names that process, never a pid reuser.
    assert_eq!(
        fd.signal(libc::SIGTERM).unwrap_err().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn gone_set_remembers_exact_identities() {
    let set = GoneSet::new();
    assert!(!set.contains(7, 1));
    set.insert(7, 1);
    assert!(set.contains(7, 1) && !set.contains(7, 2) && !set.contains(8, 1));
}

#[test]
fn boot_id_is_trimmed_and_the_raw_reader_keeps_the_file_bytes() {
    let raw = boot_id_raw();
    if raw.is_empty() {
        return;
    }
    assert!(raw.ends_with('\n'));
    assert_eq!(boot_id().as_deref(), Some(raw.trim()));
}
