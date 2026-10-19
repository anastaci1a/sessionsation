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

#[cfg(test)]
mod tests {
    use super::*;
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
