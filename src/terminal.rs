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
    // raw-path: only the file and directory names of the socket are read
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
    let stdout = raw_at(session, args, socket)?;
    decode_reply(args, &stdout)
}
/// One HERDR command; its raw stdout (`pane read` and `send-*` print no JSON).
/// HERDR errors (JSON on either stream) become `HostRejection`s.
pub(crate) fn raw_at(session: &str, args: &[&str], socket: Option<&Path>) -> Result<Vec<u8>> {
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
    Ok(output.stdout)
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

/// Close exactly one recorded HERDR pane, only while an idle shell is all that
/// runs in it (same test as auto-recover's reusable pane: foreground group is
/// the shell, one shell process). Returns `(closed, reason)`; every refusal is
/// a reason, never an error, and nothing is closed when the state is uncertain.
pub(crate) fn close_idle_pane(key: &str, socket: Option<&Path>) -> (bool, Option<String>) {
    let missing = |e: &anyhow::Error| {
        e.downcast_ref::<HostRejection>()
            .is_some_and(|r| r.0 == "pane_not_found")
    };
    let Ok((session, pane)) = target(key) else {
        return (false, Some("PANE_TARGET_INVALID".into()));
    };
    let info = match call_at(session, &["pane", "process-info", "--pane", pane], socket) {
        Ok(v) => v,
        Err(e) if missing(&e) => return (false, Some("PANE_MISSING".into())),
        Err(_) => return (false, Some("PANE_STATE_UNVERIFIED".into())),
    };
    let info = &info["process_info"];
    let group_is_shell = info["foreground_process_group_id"]
        .as_u64()
        .is_none_or(|g| info["shell_pid"].as_u64() == Some(g));
    if !group_is_shell || !process_state(info).is_ok_and(|(idle, _)| idle) {
        return (
            false,
            Some("PANE_BUSY: another program is running in the pane".into()),
        );
    }
    match raw_at(session, &["pane", "close", pane], socket) {
        Ok(_) => (true, None),
        Err(e) if missing(&e) => (false, Some("PANE_MISSING".into())),
        Err(_) => (false, Some("PANE_CLOSE_FAILED".into())),
    }
}

pub(crate) fn process_state(info: &Value) -> Result<(bool, u32)> {
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
        // A restored shell may hold typed-but-unsubmitted text that would be prepended to the
        // command. The foreground is the bare shell (checked above); best effort.
        let _ = call_at(
            session,
            &["pane", "send-keys", id, "ctrl+e", "ctrl+u"],
            socket,
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
mod tests;
