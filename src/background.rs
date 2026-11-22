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
        "creating_terminal" | "renaming" | "dispatching" | "starting_provider" => Some((
            "uncertain",
            "Worker exited during terminal handoff. Inspect the original request and destination; do not retry automatically.",
        )),
        _ => None,
    }
}

pub(crate) fn detach(command: &mut Command) -> &mut Command {
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("CODEX_HOME")
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .env_remove("CODEX_ACCESS_TOKEN")
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
}
