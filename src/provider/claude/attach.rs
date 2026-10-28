//! "Add to HERDR" for a running Claude session that has no HERDR terminal.
//!
//! Only Claude Code's own supported mechanism is used: a background-daemon
//! session (registry `kind:"bg"` with a `jobId`) is opened with
//! `claude attach <job>` in a new HERDR tab. That is a viewer client of the
//! already-running session; no agent process is started or restarted. An
//! interactive session belongs to the terminal it runs in (SSH, tmux, another
//! emulator) and cannot be moved.
use super::{Observation, observe};
use crate::{
    catalog,
    paths::Paths,
    provider::{ProviderId, claude},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    time::Duration,
};

pub const NOT_RUNNING: &str = "SESSION_NOT_RUNNING: a stopped session opens with Start Session (resume), which accepts a target Space and name";
pub const ALREADY_IN_HERDR: &str =
    "ALREADY_IN_HERDR: the session already has a verified HERDR terminal; focus it";
pub const INTERACTIVE_ELSEWHERE: &str = "INTERACTIVE_ELSEWHERE: an interactive Claude session belongs to the terminal it runs in (for example SSH or tmux) and cannot be moved into HERDR; stop it there, then use Start Session";
pub const ATTACHED_ELSEWHERE: &str = "ATTACHED_ELSEWHERE: a verified terminal is already attached to this background session; close it first";
pub const CODEX_NOT_MOVABLE: &str = "CODEX_TERMINAL_NOT_MOVABLE: a Codex terminal outside HERDR (tmux or another terminal) owns its process and cannot be moved; a conversation held only by its app-server opens with Start Session, which reattaches it";

fn live(a: &catalog::Attachment) -> bool {
    !crate::external::attachment_definitely_gone(a.pid, a.process_start)
}

/// Per-session `add_to_herdr` capability and reason for a Claude session.
pub fn capability(s: &catalog::Session, o: &Observation) -> (bool, Option<&'static str>) {
    if o.availability != "running" {
        return (false, Some(NOT_RUNNING));
    }
    match &s.attachment {
        Some(a) if a.host == "herdr" && live(a) => return (false, Some(ALREADY_IN_HERDR)),
        _ => {}
    }
    if o.native_kind.as_deref() != Some("bg") || o.job_id.is_none() {
        return (false, Some(INTERACTIVE_ELSEWHERE));
    }
    if s.attachment.as_ref().is_some_and(live) {
        return (false, Some(ATTACHED_ELSEWHERE));
    }
    (true, None)
}

/// Codex: HERDR terminals move with `move-space`; others cannot be moved.
pub fn codex_capability(running: bool, in_herdr: bool) -> (bool, &'static str) {
    if !running {
        (false, NOT_RUNNING)
    } else if in_herdr {
        (false, ALREADY_IN_HERDR)
    } else {
        (false, CODEX_NOT_MOVABLE)
    }
}

fn host(session: &str, socket: Option<&Path>, args: &[&str]) -> Result<Value> {
    crate::terminal::call_at(session, args, socket)
}

/// The admitted request, checked again by the pane step before it attaches.
pub struct Request<'a> {
    pub session: &'a str,
    pub expected_generation: &'a str,
    pub herdr_session: &'a str,
    pub herdr_socket: Option<&'a Path>,
    pub space: &'a str,
    pub name: Option<&'a str>,
}

/// Open a running background Claude session in a new tab of an existing HERDR
/// Space. Success is reported only after the new pane's foreground process is
/// verified as the `claude attach` client recorded for this session.
pub async fn add_to_herdr(paths: &Paths, r: Request<'_>) -> Result<Value> {
    crate::paths::validate_name(r.herdr_session)?;
    ensure!(
        r.herdr_socket.is_none_or(Path::is_absolute),
        "INVALID_HOST_SOCKET"
    );
    let s = catalog::resolve(paths, r.session, r.expected_generation)?;
    if s.provider == ProviderId::Codex.as_str() {
        bail!(
            "UNSUPPORTED_PROVIDER_OPERATION: {}",
            CODEX_NOT_MOVABLE
                .split_once(": ")
                .map_or(CODEX_NOT_MOVABLE, |x| x.1)
        );
    }
    ensure!(
        s.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    let account = claude::account(paths, &s.account)?;
    let (session, directory, job) = {
        // Serialize admission per conversation; released before the pane step,
        // which re-verifies identity itself (it cannot take this lock).
        let _lock = crate::external::conversation_lock(paths, &s.conversation_id)?;
        let o = observe(&account.home, &s.conversation_id, s.attachment.as_ref());
        let (supported, reason) = capability(&s, &o);
        if !supported {
            bail!("{}", reason.unwrap_or("ADD_TO_HERDR_UNSUPPORTED"));
        }
        let job = o.job_id.clone().context(INTERACTIVE_ELSEWHERE)?;
        let spaces = host(r.herdr_session, r.herdr_socket, &["workspace", "list"])?;
        ensure!(
            spaces["workspaces"]
                .as_array()
                .context("INVALID_HOST_SPACES")?
                .iter()
                .any(|w| w["workspace_id"] == r.space),
            "SPACE_NOT_FOUND"
        );
        let name = r.name.unwrap_or(&s.name);
        crate::launch::validate_session_name(name)?;
        let session = if name == s.name {
            s.clone()
        } else {
            catalog::rename(paths, &s.id, r.expected_generation, name)?
        };
        let usable = |d: &str| Path::new(d).is_absolute() && Path::new(d).is_dir();
        let directory = o
            .directory
            .clone()
            .filter(|d| usable(d))
            .or_else(|| session.directory.clone().filter(|d| usable(d)))
            .map(PathBuf::from)
            .unwrap_or_else(|| account.home.clone());
        (session, directory, job)
    };
    let dir = directory.to_str().context("INVALID_DIRECTORY")?;
    let created = host(
        r.herdr_session,
        r.herdr_socket,
        &[
            "tab",
            "create",
            "--workspace",
            r.space,
            "--cwd",
            dir,
            "--label",
            &session.name,
            "--env",
            "SESSIONSATION_LAUNCHER_BYPASS=1",
            "--no-focus",
        ],
    )?;
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .context("HOST_PANE_ID_MISSING")?
        .to_owned();
    let tab = created["tab"]["tab_id"].as_str().map(str::to_owned);
    if let Some(space) = created["tab"]["workspace_id"].as_str() {
        ensure!(space == r.space, "HOST_WRONG_WORKSPACE");
    }
    let key = crate::terminal::herdr_target(r.herdr_session, &pane)?;
    let mut ready = false;
    for _ in 0..50 {
        if crate::terminal::state_at(&key, r.herdr_socket).is_ok_and(|(idle, _)| idle) {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    ensure!(ready, "HERDR_SHELL_UNAVAILABLE: {key}");
    let q = crate::terminal::shell_quote;
    let command = format!(
        "cd -- {} && exec env -u TMUX -u TMUX_PANE SESSIONSATION_HERDR_SESSION={} {} --data-dir {} claude-attach {} --expected-generation {}",
        q(dir),
        q(r.herdr_session),
        q(&std::env::current_exe()?.to_string_lossy()),
        q(&paths.root.to_string_lossy()),
        q(&session.id),
        q(&session.generation()),
    );
    host(
        r.herdr_session,
        r.herdr_socket,
        &["pane", "run", &pane, &command],
    )?;
    let base = json!({"session_id":session.id,"host":"herdr","herdr_session":r.herdr_session,
        "space_id":r.space,"tab_id":tab,"pane_id":pane,"target":key,"name":session.name,
        "job_id":job,"starts_agent":false,"sends_input":false});
    for _ in 0..150 {
        if let Some(current) = catalog::list(paths)?
            .into_iter()
            .find(|c| c.id == session.id)
            && let Some(a) = &current.attachment
            && a.target == key
            && attach_client(a, &job)
            && crate::terminal::state_at(&key, r.herdr_socket)
                .is_ok_and(|(idle, pid)| !idle && pid == a.pid)
        {
            let mut value = base;
            value["state"] = json!("attached");
            value["generation"] = json!(current.generation());
            value["attachment"] = json!(a);
            return Ok(value);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Never re-run or start another client: the caller refreshes and inspects.
    let mut value = base;
    value["state"] = json!("attach_unconfirmed");
    value["generation"] = Value::Null;
    value["attachment"] = Value::Null;
    Ok(value)
}

/// The exact process is (now) `claude attach <job>`.
fn attach_client(a: &catalog::Attachment, job: &str) -> bool {
    live(a)
        && std::fs::read(format!("/proc/{}/cmdline", a.pid)).is_ok_and(|c| {
            let args: Vec<_> = c.split(|b| *b == 0).collect();
            args.windows(2)
                .any(|w| w[0] == b"attach" && w[1] == job.as_bytes())
        })
}

/// Pane step (hidden `claude-attach`): re-verify, record this exact terminal,
/// then exec `claude attach <job>` in place (same PID and start time).
pub fn exec_attach(paths: &Paths, id: &str, generation: &str) -> Result<()> {
    let session = catalog::resolve(paths, id, generation)?;
    ensure!(
        session.provider == ProviderId::Claude.as_str(),
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
    let account = claude::account(paths, &session.account)?;
    crate::accounts::ensure_available(paths, &session.account)?;
    ensure!(account.enabled, "ACCOUNT_UNAVAILABLE: account is disabled");
    let config = crate::store::load_config(paths)?;
    let o = observe(
        &account.home,
        &session.conversation_id,
        session.attachment.as_ref(),
    );
    let (supported, reason) = capability(&session, &o);
    if !supported {
        bail!("{}", reason.unwrap_or("ADD_TO_HERDR_UNSUPPORTED"));
    }
    let job = o.job_id.clone().context(INTERACTIVE_ELSEWHERE)?;
    let attachment = crate::external::current_attachment(paths)?;
    ensure!(
        attachment.as_ref().is_some_and(|a| a.host == "herdr"),
        "SESSION_NOT_IN_HERDR: run from the HERDR pane created for this session"
    );
    catalog::move_attachment(paths, &session, attachment)?;
    let mut command = claude::command(&config, &account.home);
    command.args(["attach", &job]);
    Err(command.exec()).context("could not run claude attach")
}
