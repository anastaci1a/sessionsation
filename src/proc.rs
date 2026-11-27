//! Processes. [`identity`] owns exact process identity (pid plus `/proc/<pid>/stat` start
//! time), liveness, pidfd signalling, termination and the boot id (guard role `identity`).

pub mod identity {
    use anyhow::{Context, Result, ensure};
    use std::{
        collections::BTreeSet,
        io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        sync::Mutex,
    };

    /// The kernel's id of the current host boot.
    pub const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";

    /// One process as `/proc/<pid>/stat` reports it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Stat {
        pub pid: u32,
        pub parent: u32,
        pub state: char,
        /// Field 22, clock ticks after boot: with the pid, the process's identity.
        pub start_time: u64,
    }
    impl Stat {
        /// Neither a zombie (`Z`) nor dead (`X`).
        pub fn running(&self) -> bool {
            !matches!(self.state, 'Z' | 'X')
        }
    }

    /// `/proc/<pid>/stat`. A missing process is the plain [`io::Error`] (`NotFound`), so
    /// callers can tell it from a malformed stat.
    pub fn stat(pid: u32) -> Result<Stat> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        parse(pid, &stat)
    }
    pub(super) fn parse(pid: u32, stat: &str) -> Result<Stat> {
        // The command name (field 2) may hold spaces and `)`; the fields after the last `) `
        // start at field 3 (state).
        let fields: Vec<_> = stat
            .rsplit_once(") ")
            .context("invalid process stat")?
            .1
            .split_whitespace()
            .collect();
        ensure!(fields.len() > 19, "short process stat");
        Ok(Stat {
            pid,
            state: fields[0].chars().next().context("missing process state")?,
            parent: fields[1].parse()?,
            start_time: fields[19].parse()?,
        })
    }
    /// `(state, start time)`; None when the process does not exist or its stat is unreadable.
    pub fn proc_start(pid: u32) -> Option<(char, u64)> {
        stat(pid).ok().map(|s| (s.state, s.start_time))
    }
    /// Start time of a process, for pid reuse checks.
    pub fn start_time(pid: u32) -> Result<u64> {
        Ok(stat(pid)?.start_time)
    }

    /// The exact process (`pid` started at `start`) runs: it exists with that start time
    /// (the pid was not reused) and is neither a zombie nor dead.
    pub fn alive(pid: u32, start: u64) -> bool {
        stat(pid).is_ok_and(|s| s.start_time == start && s.running())
    }
    /// The exact process no longer runs (missing, unreadable, reused, zombie or dead): `!alive`.
    pub fn gone(pid: u32, start: u64) -> bool {
        !alive(pid, start)
    }
    /// [`alive`] for a start time recorded as decimal text (Claude's registry `procStart`):
    /// the text must equal the kernel's start time as written by the kernel.
    pub fn alive_recorded(pid: u32, start: &str) -> bool {
        stat(pid).is_ok_and(|s| s.running() && s.start_time.to_string() == start)
    }
    /// `pid` still names the process started at `start`; unlike [`alive`] a zombie counts.
    pub fn same(pid: u32, start: u64) -> bool {
        stat(pid).is_ok_and(|s| s.start_time == start)
    }

    /// Identities proven gone (an identity never comes back, so `/proc` need not be read again).
    #[derive(Default)]
    pub struct GoneSet(Mutex<BTreeSet<(u32, u64)>>);
    impl GoneSet {
        pub const fn new() -> Self {
            Self(Mutex::new(BTreeSet::new()))
        }
        pub fn contains(&self, pid: u32, start: u64) -> bool {
            self.0.lock().is_ok_and(|g| g.contains(&(pid, start)))
        }
        pub fn insert(&self, pid: u32, start: u64) {
            if let Ok(mut g) = self.0.lock() {
                g.insert((pid, start));
            }
        }
    }

    /// A process handle (pidfd): a signal through it never reaches a later process reusing the
    /// pid. Open, then re-verify the identity, then [`Pidfd::signal`]. Closed on drop.
    #[derive(Debug)]
    pub struct Pidfd(OwnedFd);
    impl Pidfd {
        pub fn open(pid: u32) -> io::Result<Self> {
            // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::c_int, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the kernel returned a new descriptor that nothing else owns.
            Ok(Self(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) }))
        }
        /// Sends `signal` to the process this handle names (`ESRCH` once it has exited).
        pub fn signal(&self, signal: libc::c_int) -> io::Result<()> {
            // SAFETY: a valid pidfd, a signal number and no siginfo.
            let sent = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    signal,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if sent < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    /// What [`signal_exact`] did.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Signalled {
        Sent,
        /// The exact process had already gone (missing, reused, zombie or dead); nothing sent.
        Gone,
    }
    /// Why [`signal_exact`] could not finish: opening the handle or sending (other than `ESRCH`).
    #[derive(Debug)]
    pub enum SignalError {
        Open(io::Error),
        Send(io::Error),
    }
    /// Sends `signal` only to the exact process `pid` started at `start`: the pidfd is
    /// opened, then the identity is re-checked through `/proc`, then the signal goes through
    /// the handle. A pid whose start time does not match is never signalled.
    pub fn signal_exact(
        pid: u32,
        start: u64,
        signal: libc::c_int,
    ) -> Result<Signalled, SignalError> {
        let fd = match Pidfd::open(pid) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(Signalled::Gone),
            Err(e) => return Err(SignalError::Open(e)),
        };
        if gone(pid, start) {
            return Ok(Signalled::Gone);
        }
        match fd.signal(signal) {
            Ok(()) => Ok(Signalled::Sent),
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(Signalled::Gone),
            Err(e) => Err(SignalError::Send(e)),
        }
    }
    /// SIGTERM to exactly the process `pid` started at `start` (never a kill; owner of every stop).
    pub fn terminate(pid: u32, start: u64) -> Result<()> {
        signal_exact(pid, start, libc::SIGTERM)
            .map(drop)
            .map_err(|e| match e {
                SignalError::Open(e) => anyhow::anyhow!(
                    "STOP_UNAVAILABLE: could not open a stable handle on process {pid}: {e}"
                ),
                SignalError::Send(e) => {
                    anyhow::anyhow!("STOP_FAILED: could not signal process {pid}: {e}")
                }
            })
    }

    /// The raw boot id file ("" when unreadable, newline kept): attachment generations hash it.
    pub fn boot_id_raw() -> String {
        std::fs::read_to_string(BOOT_ID_PATH).unwrap_or_default()
    }
    /// The current host boot, trimmed; None when unreadable or empty
    /// (`SESSIONSATION_BOOT_ID_FILE` overrides the path, for tests).
    pub fn boot_id() -> Option<String> {
        let path =
            std::env::var("SESSIONSATION_BOOT_ID_FILE").unwrap_or_else(|_| BOOT_ID_PATH.into());
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }
}

/// Contract 1.73.1: whether another live process carries session `id`, as an exact argument
/// (a `launch-run`/`launch-worker`) or as `SESSIONSATION_SESSION_ID` (a provider). `None`
/// when that cannot be shown: the process table, or a process of this user, is unreadable.
/// "This user" is any `Uid:` in `/proc/<pid>/status` (not the directory owner, which is root
/// for a non-dumpable process); an unreadable status is unknown unless the process exited.
pub fn carries_session(id: &str) -> Option<bool> {
    let env = format!("{}={id}", crate::provider::SESSION_ENV);
    let (me, uid) = (
        std::process::id().to_string(),
        unsafe { libc::geteuid() }.to_string(),
    );
    for e in std::fs::read_dir("/proc").ok()? {
        let e = e.ok()?;
        let pid = e.file_name().to_string_lossy().into_owned();
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let has = |file, want: &[u8]| {
            std::fs::read(e.path().join(file)).map(|v| v.split(|b| *b == 0).any(|a| a == want))
        };
        match (
            has("cmdline", id.as_bytes()),
            has("environ", env.as_bytes()),
        ) {
            (Ok(true), _) | (_, Ok(true)) => return Some(true),
            (Ok(false), Ok(false)) => {}
            _ => match std::fs::read_to_string(e.path().join("status")) {
                // Exited meanwhile.
                Err(x) if x.kind() == std::io::ErrorKind::NotFound => {}
                Err(x) if x.raw_os_error() == Some(libc::ESRCH) => {}
                // Another user's process (never one Sessionsation started): no Uid is ours.
                Ok(s)
                    if s.lines()
                        .find_map(|l| l.strip_prefix("Uid:"))
                        .is_some_and(|u| u.split_whitespace().all(|v| v != uid)) => {}
                _ => return None,
            },
        }
    }
    Some(false)
}

#[cfg(test)]
mod tests;
