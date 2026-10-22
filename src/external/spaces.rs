//! Move an existing HERDR terminal, never a provider process or conversation.
//! Intent precedes the host RPC; uncertain requests are reconciled, never replayed.
use super::*;
use crate::catalog::{Attachment, Session};

#[derive(clap::Args, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Target {
    #[arg(long)]
    pub session: String,
    #[arg(long)]
    pub expected_generation: String,
    #[arg(long)]
    pub space: String,
    #[arg(long)]
    pub request_id: String,
}
#[derive(Serialize, Deserialize)]
struct Operation {
    request: Target,
    session: Session,
    source: Instance,
    socket: PathBuf,
    terminal_id: String,
    #[serde(default)]
    epoch: Option<String>,
    previous_space: String,
    phase: String,
    detail: String,
    pane: Option<Value>,
    created_at: i64,
    updated_at: i64,
}
fn dir(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.root.join("space-moves");
    private_dir(&dir)?;
    Ok(dir)
}
fn file(paths: &Paths, id: &str) -> Result<PathBuf> {
    uuid::Uuid::parse_str(id).context("INVALID_REQUEST_ID")?;
    Ok(dir(paths)?.join(format!("{id}.json")))
}
fn read(paths: &Paths, id: &str) -> Result<Operation> {
    let op: Operation = serde_json::from_slice(&fs::read(file(paths, id)?)?)?;
    ensure!(
        op.request.request_id == id
            && op.request.session == op.session.id
            && op.request.expected_generation == op.session.generation()
            && op.source.thread_id == op.session.conversation_id,
        "INVALID_SPACE_MOVE"
    );
    Ok(op)
}
fn save(paths: &Paths, op: &mut Operation, phase: &str, detail: &str) -> Result<()> {
    op.phase = phase.into();
    op.detail = detail.into();
    op.updated_at = daemon::now();
    atomic_write(
        &file(paths, &op.request.request_id)?,
        &serde_json::to_vec(op)?,
    )
}
fn records(paths: &Paths) -> Result<Vec<Operation>> {
    let mut result = vec![];
    for entry in fs::read_dir(dir(paths)?)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|s| s == "json") {
            result.push(serde_json::from_slice(&fs::read(entry.path())?)?);
        }
    }
    Ok(result)
}
fn public(paths: &Paths, op: &Operation) -> Result<Value> {
    let recovering = !matches!(op.phase.as_str(), "complete" | "failed");
    Ok(
        json!({"id":op.request.request_id,"kind":"move_space","session_id":op.session.id,
        "phase":op.phase,"detail":op.detail,"created_at":op.created_at,"updated_at":op.updated_at,
        "can_cancel":false,"can_respond":false,"can_recover":recovering && crate::background::worker_stopped(&file(paths,&op.request.request_id)?.with_extension("lock"))?,
        "previous_space_id":op.previous_space,"space_id":op.request.space,"pane":op.pane}),
    )
}
pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    records(paths)?.iter().map(|op| public(paths, op)).collect()
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|p| p.exists())
}
fn host(op: &Operation, args: &[&str]) -> Result<Value> {
    let (session, _) = crate::terminal::target(&op.source.pane)?;
    crate::terminal::call_at(session, args, Some(&op.socket))
}
fn pane(value: &Value) -> &Value {
    value.get("pane").unwrap_or(value)
}
fn verify_source(source: &Instance) -> Result<()> {
    let process = process_info(source.pid)?;
    ensure!(
        process.start_time == source.start_time
            && !matches!(process.state, 'Z' | 'X')
            && process_home(source.pid)? == source.home
            && resumed_thread(source.pid)?.as_deref() == Some(&source.thread_id),
        "STALE_TARGET"
    );
    Ok(())
}
fn choices(session: &Session, source: &Instance, socket: &Path) -> Result<Value> {
    ensure!(
        !source.inactive && source.pane.starts_with("herdr/"),
        "SESSION_NOT_IN_HERDR"
    );
    verify_source(source)?;
    let (server, id) = crate::terminal::target(&source.pane)?;
    let info = crate::terminal::call_at(server, &["pane", "get", id], Some(socket))?;
    let info = pane(&info);
    ensure!(info["pane_id"] == id, "STALE_TARGET");
    ensure!(
        crate::terminal::state_at(&source.pane, Some(socket))?.1 == source.pid,
        "STALE_TARGET"
    );
    let spaces = crate::terminal::call_at(server, &["workspace", "list"], Some(socket))?;
    let list = spaces["workspaces"]
        .as_array()
        .context("INVALID_HOST_SPACES")?;
    let current = info["workspace_id"]
        .as_str()
        .context("INVALID_HOST_SPACE")?;
    let spaces: Vec<_> = list.iter().map(|s|json!({"id":s["workspace_id"],"name":s["label"],"current":s["workspace_id"]==current})).collect();
    Ok(
        json!({"session_id":session.id,"generation":session.generation(),"current_space_id":current,"spaces":spaces,"pane":info}),
    )
}
pub async fn list(paths: &Paths, id: &str, generation: &str) -> Result<Value> {
    let (session, source) = crate::api::selected(paths, id, generation).await?;
    ensure!(
        !source.inactive && source.pane.starts_with("herdr/"),
        "SESSION_NOT_IN_HERDR"
    );
    let socket = hosting::socket_for(paths, &source.pane)?.context("MISSING_HOST_SOCKET")?;
    let result = choices(&session, &source, &socket)?;
    crate::catalog::resolve(paths, id, generation)?;
    Ok(result)
}
fn finish(paths: &Paths, op: &mut Operation, current: Value) -> Result<()> {
    ensure!(
        current["terminal_id"] == op.terminal_id && current["workspace_id"] == op.request.space,
        "SPACE_MOVE_IDENTITY_MISMATCH"
    );
    let id = current["pane_id"].as_str().context("INVALID_HOST_PANE")?;
    let (server, _) = crate::terminal::target(&op.source.pane)?;
    let target = hosting::herdr_target(server, id)?;
    let alive = !attachment_definitely_gone(op.source.pid, op.source.start_time);
    let attachment = if alive {
        verify_source(&op.source)?;
        ensure!(
            crate::terminal::state_at(&target, Some(&op.socket))?.1 == op.source.pid,
            "SPACE_MOVE_IDENTITY_MISMATCH"
        );
        Some(Attachment {
            host: "herdr".into(),
            target: target.clone(),
            pid: op.source.pid,
            process_start: op.source.start_time,
        })
    } else {
        None
    };
    // Write the verified result before changing discovery, allowing read-only recovery.
    op.pane = Some(current);
    save(
        paths,
        op,
        "reconciling",
        "Updating the moved terminal's ownership; account, process and history are unchanged.",
    )?;
    hosting::rebind(
        paths,
        &op.source.pane,
        &target,
        &op.socket,
        &Binding {
            pid: op.source.pid,
            start_time: op.source.start_time,
            home: op.source.home.clone(),
            thread: op.source.thread_id.clone(),
        },
    )?;
    crate::catalog::move_attachment(paths, &op.session, attachment)?;
    save(
        paths,
        op,
        "complete",
        "Session moved to the selected Space. No agent was restarted and no message was sent.",
    )
}
pub async fn start(paths: &Paths, request: Target) -> Result<Value> {
    let record = file(paths, &request.request_id)?;
    if record.exists() {
        let op = read(paths, &request.request_id)?;
        ensure!(op.request == request, "REQUEST_ID_CONFLICT");
        return public(paths, &op);
    }
    let request_lock = crate::paths::lock_file(&record.with_extension("lock"))?;
    request_lock
        .try_lock_exclusive()
        .context("SESSION_OPERATION_IN_PROGRESS")?;
    if record.exists() {
        let op = read(paths, &request.request_id)?;
        ensure!(op.request == request, "REQUEST_ID_CONFLICT");
        return public(paths, &op);
    }
    let (session, source) =
        crate::api::selected(paths, &request.session, &request.expected_generation).await?;
    ensure!(
        !source.inactive && source.pane.starts_with("herdr/"),
        "SESSION_NOT_IN_HERDR"
    );
    let _admission = crate::lifecycle::admit(paths, &source.thread_id, Some(&request.request_id))?;
    crate::lifecycle::ensure_no_handoff(paths, &source.thread_id, None)?;
    let _runtime = messaging_lock(paths, &source.thread_id)
        .map_err(|_| anyhow::anyhow!("SESSION_OPERATION_IN_PROGRESS"))?;
    let socket = hosting::socket_for(paths, &source.pane)?.context("MISSING_HOST_SOCKET")?;
    let available = choices(&session, &source, &socket)?;
    ensure!(
        available["spaces"]
            .as_array()
            .is_some_and(|ss| ss.iter().any(|s| s["id"] == request.space)),
        "SPACE_NOT_FOUND"
    );
    crate::catalog::resolve(paths, &request.session, &request.expected_generation)?;
    let epoch = receipt_location(paths, &session.id)?.map(|(id, _)| id);
    let mut op = Operation {
        epoch,
        request,
        session,
        source,
        socket,
        terminal_id: available["pane"]["terminal_id"]
            .as_str()
            .context("INVALID_HOST_TERMINAL")?
            .into(),
        previous_space: available["current_space_id"]
            .as_str()
            .context("INVALID_HOST_SPACE")?
            .into(),
        phase: String::new(),
        detail: String::new(),
        pane: None,
        created_at: daemon::now(),
        updated_at: daemon::now(),
    };
    save(
        paths,
        &mut op,
        "admitted",
        "Preparing to move only this terminal; its process and conversation will remain in place.",
    )?;
    if op.request.space == op.previous_space {
        op.pane = Some(available["pane"].clone());
        save(
            paths,
            &mut op,
            "complete",
            "Session is already in the selected Space; nothing was changed.",
        )?;
        return public(paths, &op);
    }
    let child = crate::background::detach(&mut std::process::Command::new(
        crate::self_executable::path()?,
    ))
    .arg("--data-dir")
    .arg(&paths.root)
    .args(["space-move-worker", &op.request.request_id])
    .spawn();
    if child.is_err() {
        save(
            paths,
            &mut op,
            "failed",
            "The move worker could not start. No terminal was moved; try again.",
        )?;
    }
    public(paths, &op)
}

/// The requester may exit immediately after admission. Only this locked worker
/// dispatches a host mutation; duplicate workers never replay a recorded intent.
pub fn worker(paths: &Paths, id: &str) -> Result<()> {
    let lock = crate::paths::lock_file(&file(paths, id)?.with_extension("lock"))?;
    lock.lock_exclusive()?;
    let mut op = read(paths, id)?;
    if op.phase != "admitted" {
        return Ok(());
    }
    if let Err(error) = execute(paths, &mut op) {
        let dispatched = op.phase != "admitted";
        let detail = if dispatched {
            "The move may have happened but could not be recorded. Use Recover to inspect the original terminal; do not submit another move.".into()
        } else {
            format!(
                "The move could not start: {error}. Refresh the session and destination before trying again."
            )
        };
        save(
            paths,
            &mut op,
            if dispatched { "uncertain" } else { "failed" },
            &detail,
        )?;
    }
    Ok(())
}

fn execute(paths: &Paths, op: &mut Operation) -> Result<()> {
    let _admission =
        crate::lifecycle::admit(paths, &op.source.thread_id, Some(&op.request.request_id))?;
    crate::lifecycle::ensure_no_handoff(paths, &op.source.thread_id, None)?;
    let _runtime = messaging_lock(paths, &op.source.thread_id)
        .map_err(|_| anyhow::anyhow!("SESSION_OPERATION_IN_PROGRESS"))?;
    crate::catalog::resolve(paths, &op.request.session, &op.request.expected_generation)?;
    let available = choices(&op.session, &op.source, &op.socket)?;
    ensure!(
        available["pane"]["terminal_id"] == op.terminal_id
            && available["current_space_id"] == op.previous_space,
        "STALE_TARGET"
    );
    ensure!(
        available["spaces"]
            .as_array()
            .is_some_and(|ss| ss.iter().any(|s| s["id"] == op.request.space)),
        "SPACE_NOT_FOUND"
    );
    save(
        paths,
        op,
        "dispatching",
        "Moving the existing terminal into a new tab in the selected Space. No provider command is sent.",
    )?;
    let (_, id) = crate::terminal::target(&op.source.pane)?;
    let moved = host(
        op,
        &[
            "pane",
            "move",
            id,
            "--new-tab",
            "--workspace",
            &op.request.space,
            "--no-focus",
        ],
    );
    match moved {
        Ok(result) if result["move_result"]["changed"] == false => {
            save(
                paths,
                op,
                "failed",
                "HERDR left the pane unchanged. Unzoom its tab if needed, refresh and choose the move again.",
            )?;
        }
        Ok(result) => {
            let result = &result["move_result"];
            let verified = if result["previous_pane_id"] == id {
                finish(paths, op, result["pane"].clone())
            } else {
                Err(anyhow::anyhow!("SPACE_MOVE_IDENTITY_MISMATCH"))
            };
            if verified.is_err() {
                save(
                    paths,
                    op,
                    "uncertain",
                    "The terminal move is not fully confirmed. It will not be resent. Use Recover to reconcile the original terminal and ownership.",
                )?;
            }
        }
        Err(error) if crate::terminal::move_rejected_before_effect(&error) => {
            save(
                paths,
                op,
                "failed",
                "HERDR rejected the move before changing the terminal. Its source or destination no longer exists; refresh the choices.",
            )?;
        }
        Err(_) => {
            save(
                paths,
                op,
                "uncertain",
                "HERDR did not confirm the move. It may already have happened; it will not be resent. Use Recover to inspect the original terminal.",
            )?;
        }
    }
    Ok(())
}
pub fn recover(paths: &Paths, id: &str) -> Result<Value> {
    let lock = crate::paths::lock_file(&file(paths, id)?.with_extension("lock"))?;
    lock.try_lock_exclusive()
        .context("SESSION_OPERATION_IN_PROGRESS")?;
    let mut op = read(paths, id)?;
    if matches!(op.phase.as_str(), "complete" | "failed") {
        return public(paths, &op);
    }
    let _admission = crate::lifecycle::admit(paths, &op.source.thread_id, Some(id))?;
    crate::lifecycle::ensure_no_handoff(paths, &op.source.thread_id, None)?;
    let _runtime = messaging_lock(paths, &op.source.thread_id)
        .map_err(|_| anyhow::anyhow!("SESSION_OPERATION_IN_PROGRESS"))?;
    if op.phase == "admitted" {
        save(
            paths,
            &mut op,
            "failed",
            "The requester exited before terminal dispatch. No move was sent; refresh before choosing a new action.",
        )?;
    } else {
        // No mutation RPC in recovery. Match the stable terminal, not an old pane alias.
        let all = host(&op, &["pane", "list", "--workspace", &op.request.space])?;
        let candidates: Vec<_> = all["panes"]
            .as_array()
            .context("INVALID_HOST_PANES")?
            .iter()
            .filter(|p| p["terminal_id"] == op.terminal_id)
            .cloned()
            .collect();
        if candidates.len() == 1 {
            if finish(paths, &mut op, candidates[0].clone()).is_err() {
                save(
                    paths,
                    &mut op,
                    "uncertain",
                    "The terminal exists in the destination but ownership is not verified. No move was replayed; keep this operation for reconciliation.",
                )?;
            }
        } else {
            save(
                paths,
                &mut op,
                "uncertain",
                "The original terminal is not uniquely confirmed in the requested Space. No move was replayed; inspect its host before another lifecycle action.",
            )?;
        }
    }
    public(paths, &op)
}

/// A new launch/resume establishes a new placement epoch even if a restarted
/// HERDR reuses its pane IDs. Never apply an earlier terminal's move history.
pub(crate) fn receipt_location(paths: &Paths, id: &str) -> Result<Option<(String, Value)>> {
    let mut latest = None;
    for directory in ["launches", "resumptions"] {
        let root = paths.root.join(directory);
        if !root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() || entry.path().extension().is_none_or(|e| e != "json")
            {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(&fs::read(entry.path())?) else {
                continue;
            };
            if (record["session_id"] != id && record["request"]["session_id"] != id)
                || !record["workspace_id"].is_string()
                || !matches!(record["phase"].as_str(), Some("running" | "attached"))
            {
                continue;
            }
            let Some(epoch) = record["request"]["request_id"].as_str() else {
                continue;
            };
            let at = (
                record["updated_at"].as_i64().unwrap_or(0),
                entry
                    .metadata()?
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
            );
            if latest.as_ref().is_none_or(|(old, _, _)| at > *old) {
                latest = Some((
                    at,
                    epoch.to_owned(),
                    json!({"session":record["request"]["host"]["session"],"socket":record["request"]["host"]["socket"],"workspace_id":record["workspace_id"],"tab_id":record["tab_id"],"pane_id":record["pane_id"]}),
                ));
            }
        }
    }
    Ok(latest.map(|(_, epoch, location)| (epoch, location)))
}

/// Replay only verified placement provenance, never the host operation itself.
pub(crate) fn follow_location(
    paths: &Paths,
    id: &str,
    mut location: Value,
    epoch: Option<&str>,
) -> Result<Value> {
    let records = records(paths)?;
    let mut after = None;
    loop {
        let mut matches: Vec<_> = records
            .iter()
            .filter(|r| {
                r.phase == "complete"
                    && epoch.is_none_or(|e| r.epoch.as_deref() == Some(e))
                    && after.is_none_or(|revision| r.session.revision > revision)
                    && r.session.id == id
                    && r.pane
                        .as_ref()
                        .is_some_and(|p| p["pane_id"] != location["pane_id"])
                    && crate::terminal::target(&r.source.pane)
                        .is_ok_and(|(s, p)| location["session"] == s && location["pane_id"] == p)
                    && location["socket"] == json!(r.socket)
            })
            .collect();
        matches.sort_by_key(|r| r.session.revision);
        ensure!(
            matches.len() < 2 || matches[0].session.revision != matches[1].session.revision,
            "SPACE_MOVE_LOCATION_CONFLICT"
        );
        let Some(op) = matches.first() else {
            return Ok(location);
        };
        after = Some(op.session.revision);
        let p = op.pane.as_ref().unwrap();
        location["terminal_id"] = p["terminal_id"].clone();
        location["workspace_id"] = p["workspace_id"].clone();
        location["tab_id"] = p["tab_id"].clone();
        location["pane_id"] = p["pane_id"].clone();
    }
}

/// Retain positive placement for sessions enrolled before launch-receipt support.
pub(crate) fn last_location(paths: &Paths, id: &str) -> Result<Value> {
    let all = records(paths)?;
    let Some(op) = all
        .iter()
        .filter(|op| op.phase == "complete" && op.session.id == id && op.pane.is_some())
        .max_by_key(|op| op.session.revision)
    else {
        return Ok(Value::Null);
    };
    let (server, _) = crate::terminal::target(&op.source.pane)?;
    let p = op.pane.as_ref().unwrap();
    Ok(
        json!({"session":server,"socket":op.socket,"terminal_id":p["terminal_id"],"workspace_id":p["workspace_id"],"tab_id":p["tab_id"],"pane_id":p["pane_id"]}),
    )
}

/// Moved terminals retain old IDs in their inherited environment. Resolve only
/// our verified move chain, then confirm the host's caller alias and foreground PID.
pub(super) fn caller_target(
    paths: &Paths,
    server: &str,
    old_pane: &str,
    pid: u32,
) -> Result<String> {
    let old = hosting::herdr_target(server, old_pane)?;
    let Some(socket) = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from) else {
        return Ok(old);
    };
    let mut location = json!({"session":server,"socket":socket,"pane_id":old_pane});
    // Placement provenance is per session, but the old pane is unique in a host.
    let all = records(paths)?;
    if !all
        .iter()
        .any(|op| op.phase == "complete" && op.source.pane == old && op.socket == socket)
    {
        return Ok(old);
    }
    let info = crate::terminal::call_at(server, &["pane", "get", old_pane], Some(&socket))?;
    let Some(op) = all.iter().find(|op| {
        op.phase == "complete"
            && op.source.pane == old
            && op.socket == socket
            && pane(&info)["terminal_id"] == op.terminal_id
    }) else {
        return Ok(old);
    };
    location = follow_location(paths, &op.session.id, location, op.epoch.as_deref())?;
    let current = location["pane_id"].as_str().context("INVALID_HOST_PANE")?;
    if current == old_pane {
        return Ok(old);
    }
    ensure!(
        pane(&info)["pane_id"] == current && pane(&info)["terminal_id"] == location["terminal_id"],
        "HERDR_BINDING_MISMATCH"
    );
    let process = crate::terminal::call_at(
        server,
        &["pane", "process-info", "--pane", current],
        Some(&socket),
    )?;
    ensure!(
        process["process_info"]["foreground_processes"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(|p| p["pid"] == pid)),
        "HERDR_BINDING_MISMATCH"
    );
    hosting::herdr_target(server, current)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(paths: &Paths) -> Operation {
        let thread = uuid::Uuid::new_v4().to_string();
        let source:Instance=serde_json::from_value(json!({"pane":"herdr/fixture/w1:p1","tmux_session":"fixture","pid":123,"start_time":456,"cwd":"/tmp/example","thread_id":thread,"rollout":"/tmp/example/history.jsonl","home":"/tmp/example/home","binary":"/bin/false","resume_options":[]})).unwrap();
        let session = crate::catalog::record_launch(
            paths,
            "example",
            &thread,
            "fixture",
            Some(Attachment {
                host: "herdr".into(),
                target: source.pane.clone(),
                pid: source.pid,
                process_start: source.start_time,
            }),
        )
        .unwrap();
        Operation {
            request: Target {
                session: session.id.clone(),
                expected_generation: session.generation(),
                space: "w2".into(),
                request_id: uuid::Uuid::new_v4().to_string(),
            },
            session,
            source,
            socket: paths.root.join("owned.sock"),
            terminal_id: "terminal-fixture".into(),
            epoch: None,
            previous_space: "w1".into(),
            phase: String::new(),
            detail: String::new(),
            pane: None,
            created_at: 1,
            updated_at: 1,
        }
    }
    #[test]
    fn pre_dispatch_crash_releases_only_its_reservation_without_host_rpc() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let mut op = fixture(&paths);
        save(&paths, &mut op, "admitted", "prepared").unwrap();
        assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_err());
        assert_eq!(
            recover(&paths, &op.request.request_id).unwrap()["phase"],
            "failed"
        );
        assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_ok());
        assert_eq!(
            crate::catalog::list(&paths).unwrap()[0].generation(),
            op.session.generation()
        );
    }
    #[test]
    fn worker_refuses_changed_target_before_host_dispatch_and_releases_reservation() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let mut op = fixture(&paths);
        save(&paths, &mut op, "admitted", "prepared").unwrap();
        crate::catalog::rename(&paths, &op.session.id, &op.session.generation(), "Changed")
            .unwrap();
        worker(&paths, &op.request.request_id).unwrap();
        let failed = read(&paths, &op.request.request_id).unwrap();
        assert_eq!(failed.phase, "failed");
        assert!(failed.detail.contains("STALE_TARGET"), "{}", failed.detail);
        assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_ok());
    }

    #[test]
    fn duplicate_workers_never_replay_dispatch_or_terminal_records() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let mut op = fixture(&paths);
        for phase in [
            "dispatching",
            "reconciling",
            "uncertain",
            "complete",
            "failed",
        ] {
            save(&paths, &mut op, phase, "must remain unchanged").unwrap();
            let original = fs::read(file(&paths, &op.request.request_id).unwrap()).unwrap();
            worker(&paths, &op.request.request_id).unwrap();
            assert_eq!(
                fs::read(file(&paths, &op.request.request_id).unwrap()).unwrap(),
                original
            );
        }
    }

    #[tokio::test]
    async fn original_request_is_observation_only_and_changed_payload_never_replays() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let mut op = fixture(&paths);
        save(&paths, &mut op, "dispatching", "unconfirmed").unwrap();
        assert_eq!(
            start(&paths, op.request.clone()).await.unwrap()["phase"],
            "dispatching"
        );
        let mut changed = op.request.clone();
        changed.space = "w3".into();
        assert!(
            start(&paths, changed)
                .await
                .unwrap_err()
                .to_string()
                .contains("REQUEST_ID_CONFLICT")
        );
        assert_eq!(
            read(&paths, &op.request.request_id).unwrap().phase,
            "dispatching"
        );
        assert!(crate::lifecycle::admit(&paths, &op.source.thread_id, None).is_err());
    }
    #[test]
    fn saved_location_follows_verified_moves_without_rewriting_launch_receipts() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(Some(d.path().join("state"))).unwrap();
        let mut op = fixture(&paths);
        let id = op.session.id.clone();
        let original = json!({"session":"fixture","socket":op.socket,"workspace_id":"w1","pane_id":"w1:p1","tab_id":"w1:t1"});
        op.pane = Some(
            json!({"pane_id":"w2:p2","workspace_id":"w2","tab_id":"w2:t2","terminal_id":op.terminal_id}),
        );
        save(&paths, &mut op, "complete", "done").unwrap();
        let mut next = fixture(&paths);
        next.session = op.session.clone();
        next.session.revision += 1;
        next.request.session = id.clone();
        next.request.expected_generation = next.session.generation();
        next.source = op.source.clone();
        next.source.pane = "herdr/fixture/w2:p2".into();
        next.request.space = "w3".into();
        next.pane = Some(
            json!({"pane_id":"w3:p3","workspace_id":"w3","tab_id":"w3:t3","terminal_id":op.terminal_id}),
        );
        save(&paths, &mut next, "uncertain", "not confirmed").unwrap();
        assert_eq!(
            follow_location(&paths, &id, original.clone(), None).unwrap()["workspace_id"],
            "w2"
        );
        save(&paths, &mut next, "complete", "done").unwrap();
        assert_eq!(
            follow_location(&paths, &id, original.clone(), None).unwrap()["workspace_id"],
            "w3"
        );
        let mut back = fixture(&paths);
        back.session = next.session.clone();
        back.session.revision += 1;
        back.request.session = id.clone();
        back.request.expected_generation = back.session.generation();
        back.source = next.source.clone();
        back.source.pane = "herdr/fixture/w3:p3".into();
        back.request.space = "w1".into();
        back.pane = Some(
            json!({"pane_id":"w1:p1","workspace_id":"w1","tab_id":"w1:t1","terminal_id":op.terminal_id}),
        );
        save(&paths, &mut back, "complete", "returned").unwrap();
        assert_eq!(
            follow_location(&paths, &id, original.clone(), None).unwrap()["workspace_id"],
            "w1"
        );
        assert_eq!(last_location(&paths, &id).unwrap()["workspace_id"], "w1");
        assert_eq!(
            follow_location(&paths, &id, original.clone(), Some("newer-resume-request")).unwrap(),
            original,
            "reused pane IDs in a later launch must not follow old moves"
        );
        assert_eq!(original["workspace_id"], "w1");
        let unrelated = json!({"session":"other","socket":"/other.sock","pane_id":"w1:p1"});
        assert_eq!(
            follow_location(&paths, &id, unrelated.clone(), None).unwrap(),
            unrelated
        );
    }
}
