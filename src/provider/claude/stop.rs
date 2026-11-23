//! Stop one live interactive Claude Code session (contract 1.59.0).
//!
//! The target is the process that Claude Code's own registry
//! (`sessions/<pid>.json`, verified by [`super::live_sessions`]: exact pid, `/proc`
//! start time, config home) names as the holder of the conversation. It must
//! still be the exact process the caller observed (pid and start time), and it
//! is signalled through a pidfd whose start time is checked after it was opened
//! (`claude_switch::terminate`, the same owner the account switch uses), so a
//! reused pid is never signalled. SIGTERM only, then a bounded wait: a process
//! that has not exited is reported (`STOP_TIMEOUT`), never killed.
use super::{Live, live_sessions};
use crate::claude_switch::{gone, terminate};
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
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::{Child, Command};

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    struct Spawned(Child);
    impl Drop for Spawned {
        fn drop(&mut self) {
            // Only the process this test spawned, by its own pid.
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    impl Spawned {
        fn alive(&mut self) -> bool {
            self.0.try_wait().unwrap().is_none()
        }
    }
    /// A stand-in Claude process of `home` (its config dir is `home`).
    fn fake(home: &Path, ignore_term: bool) -> (Spawned, u32, u64) {
        let script = if ignore_term {
            "trap '' TERM; while :; do sleep 0.1; done"
        } else {
            "while :; do sleep 0.1; done"
        };
        let child = Command::new("sh")
            .args(["-c", script])
            .env("CLAUDE_CONFIG_DIR", home)
            .spawn()
            .unwrap();
        let pid = child.id();
        // Wait for the exec so that /proc shows the final process.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let start = loop {
            if let Some((_, start)) = super::super::proc_start(pid)
                && super::super::environ(pid).is_some_and(|e| {
                    e.iter()
                        .any(|(k, v)| k == "CLAUDE_CONFIG_DIR" && Path::new(v) == home)
                })
            {
                break start;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        (Spawned(child), pid, start)
    }
    fn register(home: &Path, pid: u32, start: u64, extra: serde_json::Value) {
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let mut v = json!({"pid":pid,"sessionId":ID,"procStart":start.to_string(),"kind":"interactive","status":"idle"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        std::fs::write(
            home.join("sessions").join(format!("{pid}.json")),
            v.to_string(),
        )
        .unwrap();
    }
    fn home() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        // The registry check compares canonical config homes.
        std::fs::create_dir(t.path().join("h")).unwrap();
        t
    }

    #[test]
    fn a_process_whose_start_time_does_not_match_is_never_signalled() {
        let t = home();
        let home = std::fs::canonicalize(t.path().join("h")).unwrap();
        let (mut child, pid, start) = fake(&home, false);
        register(&home, pid, start, json!({}));
        // The caller observed the same pid with another start time (a reused pid).
        let e = stop(&home, ID, (pid, start + 1), Duration::from_secs(2)).unwrap_err();
        assert!(e.to_string().starts_with("SESSION_STATE_UNVERIFIED"), "{e}");
        // The registry entry names this pid with another start time: not a live holder.
        register(&home, pid, start + 1, json!({}));
        let e = stop(&home, ID, (pid, start), Duration::from_secs(2)).unwrap_err();
        assert!(e.to_string().starts_with("SESSION_STATE_UNVERIFIED"), "{e}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(child.alive(), "nothing was signalled");
    }

    #[test]
    fn the_verified_holder_gets_sigterm_and_the_stop_waits_for_its_exit() {
        let t = home();
        let home = std::fs::canonicalize(t.path().join("h")).unwrap();
        let (mut child, pid, start) = fake(&home, false);
        register(&home, pid, start, json!({"status":"busy"}));
        let stopped = stop(&home, ID, (pid, start), Duration::from_secs(10)).unwrap();
        assert_eq!(
            stopped,
            Stopped {
                pid,
                process_start: start,
                was_busy: true
            }
        );
        let status = child.0.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        // Stopping again finds nothing to stop (the leftover entry is dead).
        let e = stop(&home, ID, (pid, start), Duration::from_secs(1)).unwrap_err();
        assert!(e.to_string().starts_with("SESSION_NOT_RUNNING"), "{e}");
        assert!(!child.alive());
    }

    #[test]
    fn a_process_that_ignores_sigterm_is_reported_and_left_running() {
        let t = home();
        let home = std::fs::canonicalize(t.path().join("h")).unwrap();
        let (mut child, pid, start) = fake(&home, true);
        register(&home, pid, start, json!({}));
        // `sh -c` with a trap: give the shell a moment to install it.
        std::thread::sleep(Duration::from_millis(200));
        let e = stop(&home, ID, (pid, start), Duration::from_millis(500)).unwrap_err();
        assert!(e.to_string().starts_with("STOP_TIMEOUT"), "{e}");
        assert!(child.alive(), "never SIGKILLed");
    }

    #[test]
    fn background_jobs_and_unregistered_processes_are_refused_unsignalled() {
        let t = home();
        let home = std::fs::canonicalize(t.path().join("h")).unwrap();
        let (mut child, pid, start) = fake(&home, false);
        register(&home, pid, start, json!({"kind":"bg","jobId":"job-1"}));
        let e = stop(&home, ID, (pid, start), Duration::from_secs(1)).unwrap_err();
        assert!(
            e.to_string().starts_with("UNSUPPORTED_PROVIDER_OPERATION"),
            "{e}"
        );
        std::fs::remove_file(home.join("sessions").join(format!("{pid}.json"))).unwrap();
        let e = stop(&home, ID, (pid, start), Duration::from_secs(1)).unwrap_err();
        assert!(e.to_string().starts_with("SESSION_STATE_UNVERIFIED"), "{e}");
        std::thread::sleep(Duration::from_millis(300));
        assert!(child.alive());
    }
}
