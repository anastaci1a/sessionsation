//! One process boundary for durable workers that outlive their requesting viewer.
use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

pub(crate) fn worker_stopped(path: &std::path::Path) -> anyhow::Result<bool> {
    use fs2::FileExt;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn interrupted_launch(phase: &str) -> Option<(&'static str, &'static str)> {
    match phase {
        "admitted" | "preparing" => Some((
            "failed",
            "Worker exited before terminal handoff. No agent was started; any created local directories are retained.",
        )),
        "creating_terminal" | "renaming" | "dispatching" | "starting_provider" | "resuming" => {
            Some((
                "uncertain",
                "Worker exited during terminal handoff. Inspect the original request and destination; do not retry automatically.",
            ))
        }
        _ => None,
    }
}

/// Mark every inherited fd above stderr close-on-exec, so a long-lived worker never keeps a
/// client's `flock` (or any other inherited descriptor) alive. CLOEXEC, not close: Rust's
/// exec-error pipe must keep working.
pub(crate) fn close_inherited_fds() -> std::io::Result<()> {
    // SAFETY: raw syscalls only; async-signal-safe, no allocation.
    unsafe {
        if libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        ) == 0
        {
            return Ok(());
        }
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        let max = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            lim.rlim_cur.min(65536) as i32
        } else {
            4096
        };
        for fd in 3..max {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
    Ok(())
}

/// Detached-worker spawn hygiene for any std command: no inherited descriptors.
pub(crate) fn no_inherited_fds(command: &mut Command) -> &mut Command {
    // SAFETY: close_inherited_fds only makes raw async-signal-safe syscalls.
    unsafe { command.pre_exec(close_inherited_fds) }
}

pub(crate) fn detach(command: &mut Command) -> &mut Command {
    no_inherited_fds(command);
    scrub(
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
}
/// Workers never inherit Codex credentials.
fn scrub(command: &mut Command) -> &mut Command {
    command
        .env_remove("CODEX_HOME")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
}

/// Like [`detach`], but the worker also leaves the caller's session (`setsid`: a new session
/// and process group), so the end of the caller's session, process group or terminal does
/// not take it down (contract 1.64.0). `stderr` is the worker's own bounded log.
pub(crate) fn detach_session(command: &mut Command, stderr: Stdio) -> &mut Command {
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            close_inherited_fds()
        });
    }
    scrub(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr),
    )
}

/// Spawn a worker whose exit status nobody consumes, without leaving a zombie.
///
/// A long-lived parent (the supervisor) that drops a `std::process::Child` never waits for it,
/// so every finished worker stays `<defunct>` until the parent exits. This hands the child to a
/// small thread that only waits. The worker keeps running independently (own process group, see
/// `detach`); dropping the handle never kills it, and if the parent exits first the child is
/// re-parented as before.
pub(crate) trait SpawnReaped {
    fn spawn_reaped(&mut self) -> std::io::Result<()>;
}

impl SpawnReaped for Command {
    fn spawn_reaped(&mut self) -> std::io::Result<()> {
        let mut child = self.spawn()?;
        let id = child.id();
        // If no thread can be made the child is leaked exactly as before; never fail the launch.
        let _ = std::thread::Builder::new()
            .name(format!("reap-{id}"))
            .spawn(move || {
                let _ = child.wait();
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
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
        detach(&mut quick)
            .args(["-c", "echo $$ > \"$1\"", "w"])
            .arg(&pidfile);
        quick.spawn_reaped().unwrap();
        let mut slow = Command::new("sh");
        detach(&mut slow)
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
        detach(&mut command)
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
    fn assert_worker_does_not_hold_lock(spawn: impl Fn(&mut Command) -> std::io::Result<()>) {
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
        assert_worker_does_not_hold_lock(|c| detach(c).spawn_reaped());
    }
    #[test]
    fn detach_session_workers_do_not_inherit_a_clients_lock() {
        assert_worker_does_not_hold_lock(|c| detach_session(c, Stdio::null()).spawn_reaped());
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
}
