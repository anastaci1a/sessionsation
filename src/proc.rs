//! Processes. [`identity`] owns exact process identity: a pid together with its
//! `/proc/<pid>/stat` start time, liveness checks on that pair, signalling
//! through a pidfd, and the host boot id (guard role `identity`).

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
    /// The exact process no longer runs: missing, unreadable, reused (another start time),
    /// a zombie or dead. Always `!alive(pid, start)`.
    pub fn gone(pid: u32, start: u64) -> bool {
        !alive(pid, start)
    }
    /// `pid` still names the process started at `start`; unlike [`alive`] a zombie counts.
    pub fn same(pid: u32, start: u64) -> bool {
        stat(pid).is_ok_and(|s| s.start_time == start)
    }

    /// Identities proven gone. An identity never comes back (a reused pid has another start
    /// time), so a gone one can be skipped without reading `/proc` again.
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
    impl Default for GoneSet {
        fn default() -> Self {
            Self::new()
        }
    }

    /// A process handle (pidfd). It names one process object, so a signal sent through it
    /// can never reach a later process that reuses the pid number. Open it first, then
    /// re-verify the identity, then [`Pidfd::signal`]: a check made before the open could
    /// be about another process by the time the signal is sent. Closed on drop.
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
        /// The exact process had already gone (no such pid, reused, zombie or dead, or it
        /// exited before the signal landed); nothing was signalled.
        Gone,
    }
    /// Why [`signal_exact`] could not finish.
    #[derive(Debug)]
    pub enum SignalError {
        /// The handle could not be opened (other than `ESRCH`).
        Open(io::Error),
        /// The signal could not be sent (other than `ESRCH`).
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

    /// The raw boot id file ("" when unreadable), newline included: the messaging
    /// attachment generation hashes it as read.
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

#[cfg(test)]
mod tests;
