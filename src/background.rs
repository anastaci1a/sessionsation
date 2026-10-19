//! One process boundary for durable workers that outlive their requesting viewer.
use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
};

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
