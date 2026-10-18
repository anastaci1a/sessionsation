//! Provider-independent terminal hosting primitives. No account or credential logic.
use crate::paths::validate_name;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::process::Command;

pub fn herdr_target(session: &str, pane: &str) -> Result<String> {
    validate_name(session)?;
    ensure!(
        pane.split_once(':').is_some_and(|(w, p)| w.starts_with('w')
            && p.starts_with('p')
            && w[1..].bytes().all(|c| c.is_ascii_digit())
            && p[1..].bytes().all(|c| c.is_ascii_digit())
            && w.len() > 1
            && p.len() > 1),
        "invalid HERDR pane ID"
    );
    Ok(format!("herdr/{session}/{pane}"))
}

pub(crate) fn target(key: &str) -> Result<(&str, &str)> {
    let (session, pane) = key
        .strip_prefix("herdr/")
        .and_then(|s| s.split_once('/'))
        .context("invalid HERDR target")?;
    ensure!(herdr_target(session, pane)? == key, "invalid HERDR target");
    Ok((session, pane))
}

pub(crate) fn call(session: &str, args: &[&str]) -> Result<Value> {
    validate_name(session)?;
    let output = Command::new("herdr")
        .args(["--session", session])
        .args(args)
        .output()
        .context("HERDR is unavailable")?;
    ensure!(
        output.status.success(),
        "HERDR request failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    decode_reply(args, &output.stdout)
}

fn decode_reply(args: &[&str], stdout: &[u8]) -> Result<Value> {
    // pane run has no stdout on successful admission in HERDR 0.9.5.
    if stdout.is_empty() && args.starts_with(&["pane", "run"]) {
        return Ok(json!({"admitted":true}));
    }
    let value: Value = serde_json::from_slice(stdout).context("HERDR returned invalid JSON")?;
    ensure!(value.get("error").is_none(), "HERDR rejected request");
    Ok(value["result"].clone())
}

pub(crate) fn state(key: &str) -> Result<(bool, u32)> {
    let (session, pane) = target(key)?;
    let result = call(session, &["pane", "process-info", "--pane", pane])?;
    process_state(&result["process_info"])
}

fn process_state(info: &Value) -> Result<(bool, u32)> {
    let shell = info["shell_pid"]
        .as_u64()
        .context("HERDR shell PID missing")? as u32;
    let processes = info["foreground_processes"]
        .as_array()
        .context("HERDR foreground processes missing")?;
    // An idle shell is a retained terminal, not a running agent. Never run a
    // replacement over an editor or an unrelated foreground command.
    if processes.len() == 1
        && processes[0]["pid"].as_u64() == Some(shell as u64)
        && matches!(
            processes[0]["name"].as_str(),
            Some("bash" | "sh" | "zsh" | "fish")
        )
    {
        return Ok((true, shell));
    }
    ensure!(processes.len() == 1, "HERDR foreground is ambiguous");
    let process = processes.first().context("HERDR foreground is unknown")?;
    Ok((
        false,
        process["pid"].as_u64().context("HERDR PID missing")? as u32,
    ))
}

pub(crate) fn retain(pane: &str, on: bool) -> Result<()> {
    if pane.starts_with("herdr/") {
        target(pane)?;
        // HERDR's parent shell remains alive after its agent exits.
        return Ok(());
    }
    tmux(&[
        "set-option",
        "-p",
        "-t",
        pane,
        "remain-on-exit",
        if on { "on" } else { "off" },
    ])?;
    Ok(())
}

pub(crate) fn relaunch(pane: &str, cwd: &str, command: &str) -> Result<()> {
    if pane.starts_with("herdr/") {
        let (session, id) = target(pane)?;
        ensure!(
            state(pane)?.0,
            "HERDR pane is not an available shell; refresh the target"
        );
        // Keep the host shell for restart and recovery. cwd is enforced here:
        // login shells can reset the directory requested at workspace creation.
        let command = format!(
            "cd -- {} && {}",
            shell_quote(cwd),
            command.strip_prefix("exec ").unwrap_or(command)
        );
        call(session, &["pane", "run", id, &command])?;
    } else {
        tmux(&["respawn-pane", "-k", "-t", pane, "-c", cwd, command])?;
    }
    Ok(())
}

pub(crate) fn tmux(args: &[&str]) -> Result<String> {
    let output = match Command::new("tmux").args(args).output() {
        Ok(output) => output,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && args.first() == Some(&"list-panes") =>
        {
            return Ok(String::new());
        }
        Err(error) => return Err(error).context("tmux unavailable"),
    };
    let error = String::from_utf8_lossy(&output.stderr);
    if args.first() == Some(&"list-panes")
        && !output.status.success()
        && (error.contains("no server running")
            || error.contains("no sessions")
            || (error.contains("error connecting") && error.contains("No such file")))
    {
        return Ok(String::new());
    }
    ensure!(
        output.status.success(),
        "tmux operation failed: {}",
        error.chars().take(200).collect::<String>().trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_admission_is_not_a_running_receipt_or_a_valid_inventory() {
        let receipt = decode_reply(&["pane", "run", "w1:p1", "command"], b"").unwrap();
        assert_eq!(receipt["admitted"], true);
        assert!(receipt.get("running").is_none());
        assert!(decode_reply(&["api", "snapshot"], b"").is_err());
        assert!(decode_reply(&["pane", "run"], br#"{"error":{"code":"no_pane"}}"#).is_err());
    }
    #[test]
    fn retained_shell_is_not_an_agent_and_exec_is_not_an_idle_shell() {
        assert_eq!(
            process_state(
                &json!({"shell_pid":42,"foreground_processes":[{"pid":42,"name":"bash"}]})
            )
            .unwrap(),
            (true, 42)
        );
        assert_eq!(
            process_state(
                &json!({"shell_pid":42,"foreground_processes":[{"pid":42,"name":"codex"}]})
            )
            .unwrap(),
            (false, 42)
        );
        assert_eq!(
            process_state(
                &json!({"shell_pid":42,"foreground_processes":[{"pid":43,"name":"vim"}]})
            )
            .unwrap(),
            (false, 43)
        );
        assert!(process_state(&json!({"shell_pid":42,"foreground_processes":[]})).is_err());
    }
    #[test]
    fn herdr_targets_are_scoped_and_validate_each_component() {
        assert_eq!(herdr_target("test", "w12:p3").unwrap(), "herdr/test/w12:p3");
        for (s, p) in [
            ("../default", "w1:p1"),
            ("default", "w:p1"),
            ("default", "w1:p1/extra"),
            ("default", "%1"),
        ] {
            assert!(herdr_target(s, p).is_err());
        }
    }
}
