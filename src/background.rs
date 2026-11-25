//! One process boundary for durable workers that outlive their requesting viewer.
use crate::paths::Paths;
use std::{
    os::unix::process::CommandExt,
    path::Path,
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

/// A worker in its own process group, stdin/stdout `/dev/null`, stderr to [`worker_log`].
pub(crate) fn detach<'a>(command: &'a mut Command, log: &Path) -> &'a mut Command {
    no_inherited_fds(command);
    scrub(command.process_group(0).stderr(worker_log(log)))
}
/// This executable's `--data-dir ROOT ARGS...` as a [`detach`]ed worker logging to `log`.
pub(crate) fn self_worker(paths: &Paths, log: &Path, args: &[&str]) -> std::io::Result<Command> {
    let mut command = Command::new(crate::self_executable::path()?);
    detach(&mut command, log)
        .arg("--data-dir")
        .arg(&paths.root)
        .args(args);
    Ok(command)
}
/// Workers never inherit Codex credentials.
fn scrub(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .env_remove("CODEX_HOME")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
}
/// A detached worker's stderr: `log`, next to its record (retired with it), started afresh
/// above 64 KiB, one line per start; `/dev/null` only if it cannot be opened (1.68.1).
pub(crate) fn worker_log(log: &Path) -> Stdio {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};
    let fresh = std::fs::metadata(log).is_ok_and(|m| m.len() > 64 * 1024);
    let mut options = std::fs::OpenOptions::new();
    options.create(true).mode(0o600).append(!fresh);
    let Ok(mut f) = options.write(true).truncate(fresh).open(log) else {
        return Stdio::null();
    };
    let _ = writeln!(
        f,
        "{} started by {}",
        crate::daemon::now(),
        std::process::id()
    );
    f.into()
}

/// Like [`detach`], but the worker also leaves the caller's session (`setsid`: a new session
/// and process group), so the end of the caller's session, process group or terminal does
/// not take it down (contract 1.64.0).
pub(crate) fn detach_session<'a>(command: &'a mut Command, log: &Path) -> &'a mut Command {
    // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            close_inherited_fds()
        });
    }
    scrub(command.stderr(worker_log(log)))
}

/// Spawn a worker whose exit status nobody consumes, without leaving a zombie.
///
/// A long-lived parent (the supervisor) that drops a `std::process::Child` never waits for it,
/// so every finished worker stays `<defunct>` until the parent exits. This hands the child to a
/// small thread that only waits. The worker keeps running independently (own process group, see
/// `detach`); dropping the handle never kills it, and if the parent exits first the child is
/// re-parented as before. Returns the worker's exact identity.
pub(crate) trait SpawnReaped {
    fn spawn_reaped(&mut self) -> std::io::Result<Worker>;
}

impl SpawnReaped for Command {
    fn spawn_reaped(&mut self) -> std::io::Result<Worker> {
        let mut child = self.spawn()?;
        let pid = child.id();
        // Not waited for yet, so the pid still names this child (a zombie at worst).
        let start = crate::proc::identity::start_time(pid).unwrap_or_default();
        // If no thread can be made the child is leaked exactly as before; never fail the launch.
        let _ = std::thread::Builder::new()
            .name(format!("reap-{pid}"))
            .spawn(move || {
                let _ = child.wait();
            });
        Ok(Worker { pid, start })
    }
}
/// A spawned worker process: pid and `/proc` start time (a launch receipt's `worker`).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Worker {
    pub pid: u32,
    pub start: u64,
}

#[cfg(test)]
mod tests;
