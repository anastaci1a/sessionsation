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
    /// Existing destination Space id (exclusive with `--new-space`).
    #[arg(
        long,
        conflicts_with_all = ["new_space", "create_space"],
        required_unless_present_any = ["new_space", "create_space"]
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<String>,
    /// Label of a destination Space to create (exclusive with `--space`).
    #[arg(
        long,
        conflicts_with_all = ["space", "create_space"],
        required_unless_present_any = ["space", "create_space"]
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_space: Option<String>,
    /// Label of a destination Space that must not exist yet (1.48.0); unlike
    /// `--new-space` an existing label is refused (`WORKSPACE_NAME_EXISTS`).
    /// Admission turns it into a checked `new_space`, so journals stay 1.47-readable.
    #[arg(long, conflicts_with_all = ["space", "new_space"])]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_space: Option<String>,
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
    /// Space ids that existed before a new-Space request was admitted.
    #[serde(default)]
    known_spaces: Vec<String>,
    /// Space this operation created (journaled before ownership is rebound).
    #[serde(default)]
    created_space: Option<String>,
    phase: String,
    detail: String,
    pane: Option<Value>,
    created_at: i64,
    updated_at: i64,
}
fn dir(paths: &Paths) -> Result<PathBuf> {
    let dir = paths.root.join(crate::layout::SPACE_MOVES);
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
    for entry in crate::paths::journal_paths(&dir(paths)?)? {
        if entry.is_file() {
            result.push(serde_json::from_slice(&fs::read(entry)?)?);
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
        "previous_space_id":op.previous_space,"space_id":destination(op),"new_space":op.request.new_space,"created_space_id":op.created_space,"pane":op.pane}),
    )
}
pub fn operations(paths: &Paths) -> Result<Vec<Value>> {
    records(paths)?.iter().map(|op| public(paths, op)).collect()
}
pub fn exists(paths: &Paths, id: &str) -> bool {
    file(paths, id).is_ok_and(|p| p.exists())
}
/// Destination Space id: the requested one, or the one this operation created.
fn destination(op: &Operation) -> Option<&str> {
    op.request.space.as_deref().or(op.created_space.as_deref())
}
/// A new-Space move must name a fresh label; never match an existing Space.
fn fresh_label(available: &Value, label: &str) -> Result<()> {
    ensure!(
        !available["spaces"]
            .as_array()
            .is_some_and(|ss| ss.iter().any(|s| s["name"].as_str().is_some_and(|n| {
                crate::catalog::scope::fold(n) == crate::catalog::scope::fold(label)
            }))),
        "WORKSPACE_NAME_EXISTS"
    );
    Ok(())
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
            && crate::layout::same_home(&process_home(source.pid)?, &source.home)
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
/// `tab rename` arguments restoring the session label on the tab created by a move.
fn relabel_args(tab: Option<&str>, name: &str) -> Option<Vec<String>> {
    let tab = tab?;
    (!name.is_empty()).then(|| vec!["tab".into(), "rename".into(), tab.into(), name.into()])
}
fn finish(paths: &Paths, op: &mut Operation, current: Value) -> Result<()> {
    ensure!(
        current["terminal_id"] == op.terminal_id,
        "SPACE_MOVE_IDENTITY_MISMATCH"
    );
    let space = current["workspace_id"]
        .as_str()
        .context("INVALID_HOST_SPACE")?
        .to_owned();
    let label = match (&op.request.space, &op.request.new_space) {
        (Some(wanted), _) => {
            ensure!(*wanted == space, "SPACE_MOVE_IDENTITY_MISMATCH");
            // The verified destination's label becomes the session's Space.
            host(op, &["workspace", "list"]).ok().and_then(|all| {
                all["workspaces"]
                    .as_array()?
                    .iter()
                    .find(|w| w["workspace_id"] == space.as_str())
                    .and_then(|w| w["label"].as_str().map(str::to_owned))
            })
        }
        (None, Some(label)) => {
            // Only a Space absent before admission, carrying the requested label.
            ensure!(
                !op.known_spaces.contains(&space),
                "SPACE_MOVE_IDENTITY_MISMATCH"
            );
            let all = host(op, &["workspace", "list"])?;
            ensure!(
                all["workspaces"].as_array().is_some_and(|ws| ws
                    .iter()
                    .any(|w| w["workspace_id"] == space.as_str() && w["label"] == label.as_str())),
                "SPACE_MOVE_IDENTITY_MISMATCH"
            );
            op.created_space = Some(space);
            Some(label.clone())
        }
        (None, None) => bail!("INVALID_SPACE_MOVE"),
    };
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
    // HERDR names the new tab by number; keep the session label (presentation only,
    // so a failed rename never fails the verified move).
    if let Some(args) = relabel_args(current["tab_id"].as_str(), &op.session.name) {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = host(op, &args);
    }
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
    crate::catalog::move_attachment(paths, &op.session, attachment, label)?;
    save(
        paths,
        op,
        "complete",
        "Session moved to the selected Space. No agent was restarted and no message was sent.",
    )
}
/// The request as admission journals it: `--create-space` becomes a checked `new_space`.
fn admitted(request: &Target) -> Target {
    let mut r = request.clone();
    if let Some(label) = r.create_space.take() {
        r.new_space = Some(label);
    }
    r
}
/// The destination's label: the existing Space's label, or the new Space's.
fn destination_label(request: &Target, available: &Value) -> Option<String> {
    match (&request.space, &request.new_space) {
        (Some(id), _) => available["spaces"]
            .as_array()?
            .iter()
            .find(|s| s["id"] == id.as_str())
            .and_then(|s| s["name"].as_str().map(str::to_owned)),
        (None, label) => label.clone(),
    }
}
/// Visible-name fallback: a Space id that is gone (HERDR or array restart) is found by
/// its name, and a new-Space name that already exists is that Space.
fn normalize_target(request: &mut Target, available: &Value) {
    let list: Vec<Value> = available["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|s| json!({"workspace_id":s["id"],"label":s["name"]}))
        .collect();
    let found = |wanted: &str| crate::external::hosting::resolve_space(&list, wanted);
    if let Some(id) = request.space.as_deref().and_then(found) {
        request.space = Some(id);
    } else if request.space.is_none()
        && let Some(id) = request.new_space.as_deref().and_then(found)
    {
        request.space = Some(id);
        request.new_space = None;
    }
}
fn check_destination(request: &Target, available: &Value) -> Result<()> {
    if let Some(space) = &request.space {
        ensure!(
            available["spaces"]
                .as_array()
                .is_some_and(|ss| ss.iter().any(|s| s["id"] == space.as_str())),
            "SPACE_NOT_FOUND"
        );
    }
    if let Some(label) = &request.new_space {
        crate::paths::validate_space_label(label)?;
        fresh_label(available, label)?;
    }
    Ok(())
}
/// The journaled answer to a repeated request id (the same request only).
fn replay(paths: &Paths, request: &Target) -> Result<Option<Value>> {
    if !file(paths, &request.request_id)?.exists() {
        return Ok(None);
    }
    let op = read(paths, &request.request_id)?;
    ensure!(op.request == admitted(request), "REQUEST_ID_CONFLICT");
    public(paths, &op).map(Some)
}
/// Admission, no handoff of this conversation and its runtime lock (released first).
fn exclusive(paths: &Paths, thread: &str, own: &str) -> Result<((File, File), File)> {
    let admission = crate::lifecycle::admit(paths, thread, Some(own))?;
    crate::lifecycle::ensure_no_handoff(paths, thread, None)?;
    let runtime = messaging_lock(paths, thread)
        .map_err(|_| anyhow::anyhow!("SESSION_OPERATION_IN_PROGRESS"))?;
    Ok((runtime, admission))
}
pub async fn start(paths: &Paths, request: Target) -> Result<Value> {
    if let Some(done) = replay(paths, &request)? {
        return Ok(done);
    }
    let record = file(paths, &request.request_id)?;
    let request_lock = crate::paths::lock_file(&record.with_extension("lock"))?;
    request_lock
        .try_lock_exclusive()
        .context("SESSION_OPERATION_IN_PROGRESS")?;
    if let Some(done) = replay(paths, &request)? {
        return Ok(done);
    }
    let (session, source) =
        crate::api::selected(paths, &request.session, &request.expected_generation).await?;
    ensure!(
        !source.inactive && source.pane.starts_with("herdr/"),
        "SESSION_NOT_IN_HERDR"
    );
    let _held = exclusive(paths, &source.thread_id, &request.request_id)?;
    let socket = hosting::socket_for(paths, &source.pane)?.context("MISSING_HOST_SOCKET")?;
    let available = choices(&session, &source, &socket)?;
    let mut request = request;
    if let Some(label) = request.create_space.take() {
        crate::paths::validate_space_label(&label)?;
        fresh_label(&available, &label)?;
        request.new_space = Some(label);
    }
    normalize_target(&mut request, &available);
    check_destination(&request, &available)?;
    // Unique in the destination Space (contract 1.48.0).
    if request.space.as_deref() != available["current_space_id"].as_str()
        && let Some(label) = destination_label(&request, &available)
    {
        ensure!(
            !crate::catalog::space_taken(paths, Some(&label), &session.name, &session.id)?,
            "SESSION_NAME_EXISTS: \"{}\" is already used in the destination Space",
            session.name
        );
    }
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
        known_spaces: available["spaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["id"].as_str().map(str::to_owned))
            .collect(),
        created_space: None,
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
    if op.request.space.as_deref() == Some(op.previous_space.as_str()) {
        op.pane = Some(available["pane"].clone());
        save(
            paths,
            &mut op,
            "complete",
            "Session is already in the selected Space; nothing was changed.",
        )?;
        return public(paths, &op);
    }
    let id = &op.request.request_id;
    let log = file(paths, id)?.with_extension("worker.log");
    let worker = crate::background::self_worker(paths, &log, &["space-move-worker", id]);
    let child = worker?.spawn_reaped();
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
    let _held = exclusive(paths, &op.source.thread_id, &op.request.request_id)?;
    crate::catalog::resolve(paths, &op.request.session, &op.request.expected_generation)?;
    let available = choices(&op.session, &op.source, &op.socket)?;
    ensure!(
        available["pane"]["terminal_id"] == op.terminal_id
            && available["current_space_id"] == op.previous_space,
        "STALE_TARGET"
    );
    check_destination(&op.request, &available)?;
    save(
        paths,
        op,
        "dispatching",
        "Moving the existing terminal into a new tab in the selected Space. No provider command is sent.",
    )?;
    let (_, id) = crate::terminal::target(&op.source.pane)?;
    // One atomic host request: for a new Space HERDR creates the Space around
    // the moved pane itself, so no initial shell pane is ever created.
    let mut args = vec!["pane", "move", id];
    match (&op.request.space, &op.request.new_space) {
        (Some(space), _) => args.extend(["--new-tab", "--workspace", space]),
        (None, Some(label)) => args.extend(["--new-workspace", "--label", label]),
        (None, None) => bail!("INVALID_SPACE_MOVE"),
    }
    args.push("--no-focus");
    let moved = host(op, &args);
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
    let _held = exclusive(paths, &op.source.thread_id, id)?;
    if op.phase == "admitted" {
        save(
            paths,
            &mut op,
            "failed",
            "The requester exited before terminal dispatch. No move was sent; refresh before choosing a new action.",
        )?;
    } else {
        // No mutation RPC in recovery. Match the stable terminal, not an old pane alias.
        let all = match &op.request.space {
            Some(space) => host(&op, &["pane", "list", "--workspace", space])?,
            None => host(&op, &["pane", "list"])?,
        };
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
    for directory in [crate::layout::LAUNCHES, crate::layout::RESUMPTIONS] {
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

/// Existing Spaces of a named HERDR server, for a session that has no HERDR
/// terminal yet (Add to HERDR, or Start Session into an existing Space).
/// Read-only: never creates a server, Space or pane.
pub fn host_list(
    paths: &Paths,
    id: &str,
    generation: &str,
    herdr_session: &str,
    socket: Option<&Path>,
) -> Result<Value> {
    let session = crate::catalog::resolve(paths, id, generation)?;
    ensure!(socket.is_none_or(Path::is_absolute), "INVALID_HOST_SOCKET");
    let spaces = crate::terminal::call_at(herdr_session, &["workspace", "list"], socket)?;
    let spaces: Vec<_> = spaces["workspaces"]
        .as_array()
        .context("INVALID_HOST_SPACES")?
        .iter()
        .map(|s| json!({"id":s["workspace_id"],"name":s["label"],"current":false}))
        .collect();
    Ok(
        json!({"session_id":session.id,"generation":session.generation(),
        "herdr_session":herdr_session,"current_space_id":null,"spaces":spaces,"pane":null}),
    )
}

#[cfg(test)]
mod tests;
