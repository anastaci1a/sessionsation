//! Stop one live interactive Claude Code session (contract 1.59.0).
//!
//! The target is the process that Claude Code's own registry
//! (`sessions/<pid>.json`, verified by [`super::live_sessions`]: exact pid, `/proc`
//! start time, config home) names as the holder of the conversation. It must
//! still be the exact process the caller observed (pid and start time), and it
//! is signalled through a pidfd whose start time is checked after it was opened
//! ([`crate::proc::identity::terminate`], the same owner the account switch uses), so a
//! reused pid is never signalled. SIGTERM only, then a bounded wait: a process
//! that has not exited is reported (`STOP_TIMEOUT`), never killed.
use super::{Live, live_sessions};
use crate::proc::identity::{gone, terminate};
use anyhow::{Result, bail, ensure};
use std::{path::Path, time::Duration};

/// Seconds Claude Code gets to exit after SIGTERM before the stop reports a timeout.
pub const STOP_SECONDS: u64 = 10;
pub const BACKGROUND: &str = "UNSUPPORTED_PROVIDER_OPERATION: a Claude background (bg) session belongs to Claude's background daemon; stop it from `claude agents`";
pub const NOT_REGISTERED: &str = "SESSION_STATE_UNVERIFIED: no verified Claude process is registered for this conversation (it may still be starting); nothing was signalled";

/// What a stop did.
#[derive(Debug, PartialEq, Eq)]
pub struct Stopped {
    pub pid: u32,
    pub process_start: u64,
    /// The registry reported a turn in progress when the signal was sent.
    pub was_busy: bool,
}

/// Why this registry holder cannot be stopped by a signal (background jobs and
/// pre-started spares belong to Claude's daemon), if so.
pub fn refusal(l: &Live) -> Option<&'static str> {
    (l.kind.as_deref().is_some_and(|k| k != "interactive") || l.job_id.is_some() || l.spare)
        .then_some(BACKGROUND)
}

/// SIGTERM the exact process `expected` (pid, start time) that holds
/// `conversation` in `home`, then wait up to `wait` for it to exit.
/// `SESSION_NOT_RUNNING` when that process already exited and nothing holds the
/// conversation; nothing is signalled on any error before the send.
pub fn stop(
    home: &Path,
    conversation: &str,
    expected: (u32, u64),
    wait: Duration,
) -> Result<Stopped> {
    uuid::Uuid::parse_str(conversation)?;
    let (pid, start) = expected;
    let live = live_sessions(home)?;
    let holders: Vec<&Live> = live
        .iter()
        .filter(|l| l.session_id == conversation && l.parked_job_id.is_none())
        .collect();
    let l = match holders.as_slice() {
        [] if gone(pid, start) => {
            bail!("SESSION_NOT_RUNNING: the Claude process has already exited")
        }
        [] => bail!("{NOT_REGISTERED}"),
        [one] => *one,
        _ => bail!("AMBIGUOUS_SESSION: several Claude processes hold this conversation"),
    };
    ensure!(
        l.pid == pid && l.process_start == start,
        "SESSION_STATE_UNVERIFIED: a different Claude process now holds this conversation; nothing was signalled"
    );
    if let Some(reason) = refusal(l) {
        bail!("{reason}");
    }
    let was_busy = l.status.as_deref() == Some("busy");
    terminate(pid, start)?;
    let deadline = std::time::Instant::now() + wait;
    while !gone(pid, start) {
        ensure!(
            std::time::Instant::now() < deadline,
            "STOP_TIMEOUT: Claude process {pid} did not exit within {}s of SIGTERM; it was not killed (retry the stop, or end it in its terminal)",
            wait.as_secs()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(Stopped {
        pid,
        process_start: start,
        was_busy,
    })
}

#[cfg(test)]
mod tests;
