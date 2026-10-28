//! Provider-independent terminal hosting primitives. No account or credential logic.
use crate::paths::validate_name;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;

/// The HERDR session a pane belongs to: the explicit SESSIONSATION_HERDR_SESSION /
/// HERDR_SESSION when set, otherwise derived from the HERDR_SOCKET_PATH that HERDR
/// always exports into panes (`<data>/herdr.sock` is the default server,
/// `<data>/sessions/<name>/herdr.sock` a named one). Never guessed from titles.
pub fn herdr_session_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(session) = get("SESSIONSATION_HERDR_SESSION").or_else(|| get("HERDR_SESSION")) {
        return Some(session);
    }
    herdr_session_from_socket(Path::new(&get("HERDR_SOCKET_PATH")?))
}
pub fn herdr_session_from_socket(socket: &Path) -> Option<String> {
    let socket = std::fs::canonicalize(socket).unwrap_or_else(|_| socket.to_path_buf());
    if socket.file_name()? != "herdr.sock" {
        return None;
    }
    let dir = socket.parent()?;
    let session = if dir.parent()?.file_name()? == "sessions" {
        dir.file_name()?.to_str()?.to_owned()
    } else {
        "default".to_owned()
    };
    validate_name(&session).ok().map(|()| session)
}
pub fn herdr_target(session: &str, pane: &str) -> Result<String> {
    validate_name(session)?;
    ensure!(
        pane.split_once(':').is_some_and(|(w, p)| w.starts_with('w')
            && p.starts_with('p')
            && w[1..].bytes().all(|c| c.is_ascii_alphanumeric())
            && p[1..].bytes().all(|c| c.is_ascii_alphanumeric())
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
    call_at(session, args, None)
}
pub(crate) fn call_at(session: &str, args: &[&str], socket: Option<&Path>) -> Result<Value> {
    validate_name(session)?;
    let mut command = Command::new("herdr");
    if let Some(socket) = socket {
        ensure!(socket.is_absolute(), "INVALID_HOST_SOCKET");
        // HERDR 0.9.5's --session recomputes a socket under the reader's XDG
        // directory, overriding HERDR_SOCKET_PATH. Keep explicit locators exact.
        command
            .env("HERDR_SOCKET_PATH", socket)
            .env("HERDR_SESSION", session);
    } else {
        command.args(["--session", session]);
    }
    command.args(args);
    let output = command.output().context("HERDR is unavailable")?;
    for bytes in [&output.stdout, &output.stderr] {
        if let Ok(v) = serde_json::from_slice::<Value>(bytes)
            && let Some(code) = v["error"]["code"].as_str()
        {
            return Err(HostRejection(code.into()).into());
        }
    }
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
    state_at(key, None)
}
pub(crate) fn state_at(key: &str, socket: Option<&Path>) -> Result<(bool, u32)> {
    let (session, pane) = target(key)?;
    let result = call_at(session, &["pane", "process-info", "--pane", pane], socket)?;
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
    relaunch_at(pane, cwd, command, None)
}
pub(crate) fn relaunch_at(
    pane: &str,
    cwd: &str,
    command: &str,
    socket: Option<&Path>,
) -> Result<()> {
    if pane.starts_with("herdr/") {
        let (session, id) = target(pane)?;
        ensure!(
            state_at(pane, socket)?.0,
            "HERDR pane is not an available shell; refresh the target"
        );
        // Keep the host shell for restart and recovery. cwd is enforced here:
        // login shells can reset the directory requested at workspace creation.
        let command = format!(
            "cd -- {} && {}",
            shell_quote(cwd),
            command.strip_prefix("exec ").unwrap_or(command)
        );
        call_at(session, &["pane", "run", id, &command], socket)?;
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

#[derive(Debug)]
struct HostRejection(String);
impl std::fmt::Display for HostRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HERDR rejected request: {}", self.0)
    }
}
impl std::error::Error for HostRejection {}
pub(crate) fn move_rejected_before_effect(error: &anyhow::Error) -> bool {
    // Verified before take_pane_for_move in HERDR v0.9.5's handle_pane_move.
    // pane_layout_unavailable/pane_move_failed can occur AFTER an effect.
    error.downcast_ref::<HostRejection>().is_some_and(|r| {
        matches!(
            r.0.as_str(),
            "pane_not_found" | "tab_not_found" | "workspace_not_found"
        )
    })
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
        assert_eq!(
            herdr_target("default", "wA:pB").unwrap(),
            "herdr/default/wA:pB"
        );
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

#[cfg(test)]
mod move_rejection_tests {
    use super::*;
    #[test]
    fn only_verified_pre_effect_codes_release_uncertain_move_intent() {
        for code in ["pane_not_found", "tab_not_found", "workspace_not_found"] {
            assert!(move_rejected_before_effect(
                &HostRejection(code.into()).into()
            ));
        }
        for code in [
            "pane_move_failed",
            "pane_layout_unavailable",
            "timeout",
            "unknown",
        ] {
            assert!(!move_rejected_before_effect(
                &HostRejection(code.into()).into()
            ));
        }
        assert!(!move_rejected_before_effect(&anyhow::anyhow!(
            "workspace_not_found"
        )));
    }
}
#[cfg(test)]
mod session_tests {
    use super::herdr_session_from_socket;
    use std::path::Path;
    #[test]
    fn herdr_session_is_derived_only_from_the_socket_layout() {
        let s = |p: &str| herdr_session_from_socket(Path::new(p));
        assert_eq!(s("/data/herdr/herdr.sock").as_deref(), Some("default"));
        assert_eq!(
            s("/data/herdr/sessions/work-1/herdr.sock").as_deref(),
            Some("work-1")
        );
        assert_eq!(s("/data/herdr/other.sock"), None);
        assert_eq!(s("/data/herdr/sessions/bad name/herdr.sock"), None);
    }
}
