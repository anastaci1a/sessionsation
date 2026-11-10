//! Durable manual moves. Every destructive step follows a verified idle boundary.
use super::*;
use crate::control::{Control, confirmed_quiescent};

pub(super) fn phase(
    paths: &Paths,
    job: &mut Job,
    name: &str,
    detail: impl Into<String>,
) -> Result<()> {
    let detail = detail.into();
    if job.phase != name || job.detail != detail {
        job.phase = name.into();
        job.detail = detail;
        save_job(paths, job)?;
    }
    Ok(())
}

fn cancelled(paths: &Paths, job: &Job) -> bool {
    paths
        .root
        .join(crate::layout::HANDOFF_JOBS)
        .join(format!("{}.cancel", job.id))
        .exists()
}

pub(super) async fn tick(paths: &Paths, job: &Job) -> Result<()> {
    ensure!(
        !cancelled(paths, job),
        "move cancelled; source history retained"
    );
    if job.phase.starts_with("waiting_") && job.expires_at().is_some_and(|at| daemon::now() > at) {
        // Nothing was stopped, so the stop gate recorded for this request goes too.
        let _ = super::catalog::clear_stop_intent_if_matches(
            paths,
            &job.source.thread_id,
            Some(&job.id),
        );
        bail!(
            "the stop request expired while waiting for the session to become idle; nothing was stopped"
        );
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

pub(super) async fn confirm(
    paths: &Paths,
    job: &mut Job,
    kind: &str,
    detail: impl Into<String>,
) -> Result<()> {
    let response = jobs_dir(paths)?.join(format!("{}.response", job.id));
    if response.exists() {
        fs::remove_file(&response)?;
    }
    job.confirmation_seq += 1;
    job.phase = kind.into();
    job.detail = detail.into();
    save_job(paths, job)?;
    loop {
        tick(paths, job).await?;
        if let Ok(raw) = fs::read(&response) {
            let value: Value = serde_json::from_slice(&raw)?;
            if value["phase"] == kind && value["sequence"] == job.confirmation_seq {
                ensure!(
                    value["approve"] == true,
                    "confirmation declined; source history retained"
                );
                fs::remove_file(&response)?;
                return Ok(());
            }
        }
    }
}

pub(super) async fn risks(paths: &Paths, job: &mut Job, detail: impl Into<String>) -> Result<()> {
    let detail = detail.into();
    if !job.risks_approved && !job.acknowledged_risks.contains(&detail) {
        confirm(paths, job, "awaiting_risk_confirmation", &detail).await?;
        job.acknowledged_risks.push(detail);
        save_job(paths, job)?;
    }
    Ok(())
}

fn pause_message(job: &Job) -> String {
    let operation = match job.kind.as_str() {
        "restart" => "A Codex restart",
        "terminate" => "Ending this session",
        "pause" => "Pausing this session",
        "directory" => "A working-directory change",
        _ => "An account switch",
    };
    let until = if job.resume_after_move {
        "Stay paused until the switcher sends the continuation message after a successful move, or the user asks you to continue."
    } else {
        "Stay paused until the user asks you to continue."
    };
    format!(
        "[codex-switcher move {}] Stop what you're doing at a safe stopping point. Pause your goal, subagents, and background tasks; do not start more work. Save a concise handoff summary and end this turn. {operation} is queued and will wait for you to become idle. {until}",
        job.id
    )
}

async fn pause(paths: &Paths, job: &mut Job, control: Option<&mut Control>) -> Result<()> {
    if !job.pause_approved {
        let detail = if job.kind == "pause" {
            "Send 'stop what you are doing and pause background tasks' to this conversation? Codex will stay open after its work becomes idle. Remaining background terminals need separate approval to stop. For a direct terminal session, clear unsent input first; the request is submitted through its composer."
        } else {
            "Send 'stop what you are doing and pause background tasks' to this conversation, then continue? Pausing is required before stopping a working session. For a direct terminal session, clear unsent input first; the request is submitted through its composer."
        };
        let detail = if job.resume_after_move {
            format!(
                "{detail} After a successful account switch, a continuation message will automatically ask Codex to resume its work."
            )
        } else {
            detail.to_owned()
        };
        confirm(paths, job, "awaiting_pause_confirmation", detail).await?;
        job.pause_approved = true;
        save_job(paths, job)?;
    }
    let message = pause_message(job);
    if let Some(control) = control {
        for file in descendants(&job.source.home, &job.source.thread_id)? {
            let id = session_meta(&file)?["id"]
                .as_str()
                .context("missing thread ID")?
                .to_owned();
            let thread = control.thread(&id, true).await?;
            if thread["status"]["type"] != "active" {
                continue;
            }
            let turn = thread["turns"]
                .as_array()
                .and_then(|turns| turns.iter().rev().find(|t| t["status"] == "inProgress"))
                .and_then(|t| t["id"].as_str())
                .context("Codex has not reported the active turn identity yet")?;
            let key = format!("{id}:{turn}");
            if job.steered_turns.contains_key(&key) {
                continue;
            }
            job.steered_turns.insert(key.clone(), "sending".into());
            save_job(paths, job)?;
            // A client ID lets the native item history identify this exact steer input.
            let client = uuid::Uuid::new_v4().to_string();
            control.request("turn/steer",json!({"threadId":id,"clientUserMessageId":client,"expectedTurnId":turn,"input":[{"type":"text","text":message}]})).await?;
            job.steered_turns.insert(key, "accepted".into());
            save_job(paths, job)?;
        }
    } else {
        if job.pause_delivery == "accepted" {
            return Ok(());
        }
        if job.pause_delivery == "sending" {
            if fs::read_to_string(&job.source.rollout)
                .is_ok_and(|s| s.contains(&format!("[codex-switcher move {}]", job.id)))
            {
                job.pause_delivery = "accepted".into();
                save_job(paths, job)?;
            } else {
                phase(
                    paths,
                    job,
                    "waiting_for_pause",
                    "Pause delivery is unconfirmed. The request will not be resent automatically. Check the terminal; cancel and retry if it did not accept the request.",
                )?;
            }
            return Ok(());
        }
        // Enter steers an active Codex turn; Tab would only queue the text.
        // Confirmation explicitly asks the user to clear any unsent input first.
        job.pause_delivery = "sending".into();
        phase(
            paths,
            job,
            "sending_pause",
            "Submitting the approved pause request through the selected terminal.",
        )?;
        let buffer = format!("switcher-{}", job.id);
        tmux(&["set-buffer", "-b", &buffer, &message])?;
        let result = tmux(&[
            "paste-buffer",
            "-p",
            "-d",
            "-b",
            &buffer,
            "-t",
            &job.source.pane,
        ]);
        if result.is_err() {
            let _ = tmux(&["delete-buffer", "-b", &buffer]);
        }
        result?;
        tmux(&["send-keys", "-t", &job.source.pane, "Enter"])?;
    }
    phase(
        paths,
        job,
        "waiting_for_idle",
        "Pause requested. Waiting for Codex and its background work to become idle.",
    )?;
    Ok(())
}

pub(super) async fn identify(paths: &Paths, job: &mut Job) -> Result<()> {
    if job.source.inactive {
        if crate::control::available(&job.source.home) {
            let mut control = Control::connect(&job.source.home).await?;
            let thread = control.thread(&job.source.thread_id, false).await?;
            if thread["status"]["type"] != "notLoaded" {
                job.source.shared_server = true;
            }
        }
        return Ok(());
    }
    loop {
        let current = live_sessions(paths)
            .await?
            .into_iter()
            .find(|i| !i.inactive && i.pane == job.source.pane)
            .context("the selected Codex pane exited; its saved history is still available")?;
        ensure!(
            current.pid == job.source.pid && current.start_time == job.source.start_time,
            "the selected pane now runs a different process; select it again"
        );
        if !job.source.thread_id.is_empty() && !current.thread_id.is_empty() {
            ensure!(
                current.thread_id == job.source.thread_id,
                "the pane changed conversations; select it again"
            );
        }
        if !current.thread_id.is_empty() {
            job.source = current;
            save_job(paths, job)?;
            return Ok(());
        }
        phase(
            paths,
            job,
            "identifying",
            "Waiting for an exact conversation identity. Open or submit the conversation in this pane; the queued move will continue when Codex publishes it.",
        )?;
        tick(paths, job).await?;
    }
}

async fn prepare_destination(paths: &Paths, job: &mut Job) -> Result<Control> {
    loop {
        tick(paths, job).await?;
        let status = daemon::call(paths, "status", json!({})).await?;
        let account = status["accounts"]
            .as_array()
            .and_then(|a| {
                a.iter().find(|a| {
                    a["name"] == job.target && a["provider"].as_str().unwrap_or("codex") == "codex"
                })
            })
            .context("destination account was removed")?;
        if account["enabled"] != true || account["auth_required"] == true {
            phase(
                paths,
                job,
                "waiting_for_account",
                "The destination needs login or is disabled. Sign in or enable it; the source stays running and this move will retry.",
            )?;
            continue;
        }
        if account["eligible"] != true {
            risks(paths,job,"Destination capacity is stale or limited. Continue anyway? The resumed conversation may need to wait for credits or a usage reset; its history will be retained.").await?;
        }
        match account_control(paths, &job.target).await {
            Ok(control) => return Ok(control),
            Err(error) => phase(
                paths,
                job,
                "waiting_for_account",
                format!(
                    "Waiting for destination preparation: {error}. The source stays running; cancel to choose another account."
                ),
            )?,
        }
    }
}

fn interrupted_or_empty(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let mut last = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            last = None;
            continue;
        };
        if v["type"] == "event_msg" {
            match v["payload"]["type"].as_str() {
                Some("task_started") => last = Some(false),
                Some("task_complete" | "turn_aborted") => last = Some(true),
                _ => {}
            }
        }
    }
    last.unwrap_or(false)
}

pub(super) async fn wait_idle(paths: &Paths, job: &mut Job) -> Result<Vec<PathBuf>> {
    let mut stable = None;
    loop {
        tick(paths, job).await?;
        if !job.source.inactive {
            let info = process_info(job.source.pid)?;
            ensure!(
                info.start_time == job.source.start_time,
                "source process was replaced"
            );
            ensure!(
                pane_state(paths, &job.source.pane)?.1 == job.source.pid
                    || child_processes(pane_state(paths, &job.source.pane)?.1)?
                        .iter()
                        .any(|p| p.pid == job.source.pid && p.start_time == job.source.start_time),
                "pane process changed"
            );
        }
        let files = descendants(&job.source.home, &job.source.thread_id)?;
        if files.len() > 1 && !matches!(job.kind.as_str(), "terminate" | "pause") {
            risks(paths,job,"This conversation has subagents. Their saved histories will be copied after they become idle, but restoring live parent/child coordination is not verified. Continue with that limitation?").await?;
        }
        let mut control = if job.source.shared_server {
            Some(Control::connect(&job.source.home).await?)
        } else {
            None
        };
        let mut busy = false;
        let mut provider_startup = false;
        let mut working = false;
        if let Some(control) = control.as_mut() {
            for file in &files {
                let meta = session_meta(file)?;
                let id = meta["id"].as_str().context("missing thread ID")?;
                let thread = control.thread_for_state(id).await?;
                if id == job.source.thread_id
                    && !job.source.inactive
                    && thread["status"]["type"] == "notLoaded"
                {
                    provider_startup = true;
                    continue;
                }
                if id == job.source.thread_id
                    && job.kind == "account"
                    && !job.source.inactive
                    && !job.resume_after_move
                    && super::catalog::stop_intent(paths, id)?.is_none()
                {
                    // Quiescent does not mean voluntarily finished: a failed turn
                    // (including quota failure) can safely move but needs continuation.
                    let failed = if thread["status"]["type"] == "systemError" {
                        confirmed_quiescent(&thread, false)
                            && thread["turns"]
                                .as_array()
                                .and_then(|turns| turns.last())
                                .is_some_and(|turn| turn["status"] == "failed")
                    } else if thread["status"]["type"] == "idle" {
                        control.request("thread/turns/list", json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":1})).await
                            .ok().is_some_and(|page| page["data"].as_array().is_some_and(|turns| turns.len()==1 && turns[0]["status"]=="failed"))
                    } else {
                        false
                    };
                    if failed {
                        job.resume_after_move = true;
                        save_job(paths, job)?;
                    }
                }
                if confirmed_quiescent(&thread, true) {
                    continue;
                }
                match thread["status"]["type"].as_str() {
                    Some("active") => {
                        busy = true;
                        working = true;
                    }
                    _ => {
                        phase(
                            paths,
                            job,
                            "waiting_for_idle",
                            "Codex has not confirmed that every conversation in this tree is idle. Waiting without stopping it.",
                        )?;
                        busy = true;
                    }
                }
            }
        } else {
            busy = !files.iter().all(|p| complete_rollout(p).unwrap_or(false));
            if busy && files.iter().all(|p| interrupted_or_empty(p)) {
                let detail = if matches!(job.kind.as_str(), "terminate" | "pause") {
                    "The last turn was interrupted. Continue with the session action? Unfinished work stays recorded and will not be replayed; review it when resuming."
                } else {
                    "The last turn was interrupted or this conversation has no completed turn. Resume its saved history anyway? Review unfinished work before continuing."
                };
                if !(job.source.inactive && job.unfinished_history_confirmed) {
                    risks(paths, job, detail).await?;
                }
                busy = false;
            }
            working = busy;
        }
        if provider_startup {
            phase(
                paths,
                job,
                "waiting_for_provider_ready",
                "Codex has not loaded this conversation yet. Complete any startup prompt in its terminal, or cancel this operation. Nothing has been stopped or switched.",
            )?;
            stable = None;
            continue;
        }
        if job.source.inactive && busy {
            if files.iter().any(|p| rollout_still_open(p).unwrap_or(true)) {
                phase(
                    paths,
                    job,
                    "waiting_for_idle",
                    "Another process is still using this saved history. Waiting for it to release the conversation.",
                )?;
                continue;
            }
            if !job.unfinished_history_confirmed {
                risks(paths,job,"This saved conversation ended during a turn. Open the recorded history anyway? Unfinished operations may need review; the switcher will not replay the interrupted request.").await?;
            }
            busy = false;
        }
        if working
            && !job.non_steering
            && !job.source.pane.starts_with("herdr/")
            && !job.source.inactive
            && job.kind == "account"
            && !job.resume_after_move
        {
            job.resume_after_move = true;
            save_job(paths, job)?;
        }
        if busy || job.pause_delivery == "sending" {
            if job.non_steering || job.source.pane.starts_with("herdr/") {
                phase(
                    paths,
                    job,
                    "waiting_for_idle",
                    "Waiting for positive idle state; no interruption or steer is sent.",
                )?;
                stable = None;
                continue;
            }
            if !job.source.inactive {
                if let Err(error) = pause(paths, job, control.as_mut()).await {
                    if error.to_string().contains("confirmation declined") || cancelled(paths, job)
                    {
                        return Err(error);
                    }
                    phase(
                        paths,
                        job,
                        "waiting_for_pause",
                        format!(
                            "Pause request needs attention: {error}. The source remains running. Cancel and retry if delivery cannot be verified."
                        ),
                    )?;
                }
            }
            stable = None;
            continue;
        }
        let lengths = files
            .iter()
            .map(|p| Ok((p.clone(), fs::metadata(p)?.len())))
            .collect::<Result<Vec<_>>>()?;
        if stable.as_ref() != Some(&lengths) {
            stable = Some(lengths);
            phase(
                paths,
                job,
                "waiting_for_idle",
                "Codex is idle. Waiting for its saved history to finish flushing.",
            )?;
            continue;
        }
        if let Some(control) = control.as_mut() {
            let mut terminals = Vec::new();
            for file in &files {
                let id = session_meta(file)?["id"]
                    .as_str()
                    .context("missing thread ID")?
                    .to_owned();
                let goal = control
                    .request("thread/goal/get", json!({"threadId":id}))
                    .await?;
                if goal["goal"]["status"] == "active" {
                    if !job.pause_approved {
                        confirm(paths,job,"awaiting_pause_confirmation","Pause this conversation's active goal and background work before continuing with the requested action?").await?;
                        job.pause_approved = true;
                        save_job(paths, job)?;
                    }
                    control
                        .request(
                            "thread/goal/set",
                            json!({"threadId":id,"status":"paused","origin":"user"}),
                        )
                        .await?;
                }
                let queue = control
                    .request("thread/queue/list", json!({"threadId":id,"limit":1}))
                    .await?;
                ensure!(
                    queue["data"].as_array().is_some_and(|q| q.is_empty()),
                    "this conversation has queued input; submit or remove it before stopping so no accepted request is lost"
                );
                terminals.extend(
                    control
                        .terminals(&id)
                        .await?
                        .into_iter()
                        .map(|t| (id.clone(), t)),
                );
            }
            if !terminals.is_empty() {
                confirm(paths,job,"awaiting_background_approval",format!("{} background terminal(s) are still running after the turn ended. Stop them and continue? Unsaved process state will be lost; declining leaves the source running.",terminals.len())).await?;
                for (id, terminal) in terminals {
                    ensure!(
                        confirmed_quiescent(&control.thread_for_state(&id).await?, false),
                        "a new turn started before background cleanup; source retained"
                    );
                    control
                        .request(
                            "thread/backgroundTerminals/terminate",
                            json!({"threadId":id,"processId":terminal["processId"]}),
                        )
                        .await?;
                }
                stable = None;
                continue;
            }
        } else if !job.source.inactive {
            let children = child_processes(job.source.pid)?;
            if !children.is_empty() {
                confirm(paths,job,"awaiting_background_approval",format!("{} attached process(es) remain after the turn ended. Stop them and continue? Unsaved background work may be lost; declining leaves Codex running.",children.len())).await?;
                ensure!(
                    complete_rollout(&job.source.rollout)?
                        || interrupted_or_empty(&job.source.rollout),
                    "the turn restarted before background cleanup"
                );
                for child in children.iter().rev() {
                    stop_exact_process(*child)?;
                    wait_stopped(*child).await?;
                }
                stable = None;
                continue;
            }
        }
        if job.source.inactive
            && control.is_none()
            && files.iter().any(|p| rollout_still_open(p).unwrap_or(true))
        {
            phase(
                paths,
                job,
                "waiting_for_idle",
                "The history is still open by another process. Waiting before copying it.",
            )?;
            continue;
        }
        return Ok(files);
    }
}

/// A history path Codex reports for the destination, resolved. Codex may report
/// the legacy (symlink) spelling of the destination home, so both sides are
/// compared by resolved path.
fn reported_history_path(reported: &str, target_home: &Path) -> Result<PathBuf> {
    let file = crate::layout::resolve_path(Path::new(reported));
    let home = crate::layout::resolve_path(target_home);
    ensure!(
        file.starts_with(home.join("sessions")) || file.starts_with(home.join("archived_sessions")),
        "Codex reported a history path outside the destination home"
    );
    Ok(file)
}
fn target_path(source: &Path, home: &Path, target_home: &Path) -> Result<PathBuf> {
    let relative: PathBuf = match source.strip_prefix(home) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => crate::layout::resolve_path(source)
            .strip_prefix(crate::layout::resolve_path(home))
            .context("history outside source account")?
            .to_path_buf(),
    };
    if relative.starts_with("sessions") {
        return Ok(target_home.join(relative));
    }
    ensure!(
        relative.starts_with("archived_sessions"),
        "unexpected conversation history location"
    );
    Ok(target_home
        .join("sessions")
        .join(relative.strip_prefix("archived_sessions")?))
}

fn transfer(paths: &Paths, job: &Job, file: &Path, target: PathBuf) -> Result<PathBuf> {
    let bytes = fs::read(file)?;
    private_dir(target.parent().context("missing destination directory")?)?;
    if target.exists() {
        ensure!(
            !rollout_still_open(&target)?,
            "The destination conversation was reopened during transfer. Its history was not replaced; restore the source and retry after closing that copy."
        );
        ensure!(
            !fs::symlink_metadata(&target)?.file_type().is_symlink(),
            "destination history is a symlink"
        );
        let old = fs::read(&target)?;
        if old == bytes {
            return Ok(target);
        }
        if !bytes.starts_with(&old) {
            ensure!(
                job.risks_approved || !job.acknowledged_risks.is_empty(),
                "destination history differs; confirmation required before replacing it"
            );
            let backup = paths
                .root
                .join(crate::layout::MOVE_BACKUPS)
                .join(&job.id)
                .join(&job.target);
            private_dir(&backup)?;
            atomic_write(
                &backup.join(target.file_name().context("missing history filename")?),
                &old,
            )?;
        }
    }
    atomic_write(&target, &bytes)?;
    Ok(target)
}

async fn perform(paths: &Paths, job: &mut Job) -> Result<()> {
    if matches!(job.kind.as_str(), "terminate" | "pause") {
        return super::session_control::perform(paths, job).await;
    }
    if job.relocation.is_some() {
        return super::relocation::perform(paths, job).await;
    }
    identify(paths, job).await?;
    if !job.source.warnings.is_empty() {
        risks(paths, job, job.source.warnings.join(" ")).await?;
    }
    if job.source.block_reason.as_deref() == Some("diverged") {
        risks(paths, job, reason_detail(Some("diverged"))).await?;
    }
    ensure!(
        job.resume_directory
            .as_deref()
            .unwrap_or(Path::new(&job.source.cwd))
            .is_dir(),
        "the working directory no longer exists; restore it and requeue the move"
    );
    let mut target = prepare_destination(paths, job).await?;
    let target_home = paths.account(&job.target)?;
    let target_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target_home.join(".switcher-move"))?;
    while target_lock.try_lock_exclusive().is_err() {
        phase(
            paths,
            job,
            "waiting_for_account",
            "Another move is preparing this account. Waiting; the source stays running.",
        )?;
        tick(paths, job).await?;
    }
    let mut files = wait_idle(paths, job).await?;
    let mut destinations = std::collections::BTreeMap::new();
    // Check destination conflicts before stopping the source.
    for file in &files {
        let mut target_file = target_path(file, &job.source.home, &target_home)?;
        let id = session_meta(file)?["id"]
            .as_str()
            .context("missing thread identity")?
            .to_owned();
        if let Ok(thread) = target.thread(&id, false).await {
            ensure!(
                thread["status"]["type"] == "notLoaded",
                "the destination already has this conversation open; close that copy before retrying"
            );
            if let Some(path) = thread["path"].as_str() {
                target_file = reported_history_path(path, &target_home)?;
            }
        }
        if target_file.exists() && !fs::read(file)?.starts_with(&fs::read(&target_file)?) {
            risks(paths,job,"The destination has a different saved history. Continue using the selected source? Its older destination copy will be backed up privately for recovery.").await?;
        }
        destinations.insert(id, target_file);
    }
    // Recheck after any confirmations: the user may have started another turn.
    files = wait_idle(paths, job).await?;
    ensure!(
        files.iter().all(|file| session_meta(file)
            .ok()
            .and_then(|m| m["id"].as_str().map(|id| destinations.contains_key(id)))
            .unwrap_or(false)),
        "The conversation gained a child while waiting. The source is unchanged; retry to include its new history."
    );
    super::history::capture(paths, job, &files).await?;
    ensure!(
        !cancelled(paths, job),
        "move cancelled before stopping source"
    );
    files = stop_source(paths, job, files).await?;
    let mut root_target = None;
    for file in &files {
        ensure!(
            !rollout_still_open(file)?,
            "source history still has an open writer"
        );
        let meta = session_meta(file)?;
        let id = meta["id"].as_str().context("missing thread identity")?;
        let mut destination = destinations[id].clone();
        if let Ok(thread) = target.thread(id, false).await {
            ensure!(
                thread["status"]["type"] == "notLoaded",
                "The destination conversation was reopened. Its history is unchanged; recovery will restore the source when safe."
            );
        }
        if crate::layout::resolve_path(&destination)
            .starts_with(crate::layout::resolve_path(&target_home).join("archived_sessions"))
        {
            // A previous visit left a native archived identity. Restore that
            // identity first, then replace its canonical rollout; a newly chosen
            // filename is rejected by Codex as a stale path for paginated history.
            target
                .request("thread/unarchive", json!({"threadId":id}))
                .await?;
            destination = crate::layout::resolve_path(Path::new(
                target.thread(id, false).await?["path"]
                    .as_str()
                    .context("destination history path missing")?,
            ));
            ensure!(
                destination.starts_with(crate::layout::resolve_path(&target_home).join("sessions")),
                "restored history path is outside destination sessions"
            );
        }
        let copied = transfer(paths, job, file, destination)?;
        destinations.insert(id.to_owned(), copied.clone());
        if id == job.source.thread_id {
            root_target = Some(copied);
        }
    }
    let root_target = root_target.context("root conversation was not copied")?;
    super::history::prepare_children(paths, job, &mut target, &destinations).await?;
    launch_target(paths, job, &target_home, &root_target, Some(&mut target)).await
}

pub(super) async fn launch_target(
    paths: &Paths,
    job: &mut Job,
    target_home: &Path,
    root_target: &Path,
    mut target: Option<&mut Control>,
) -> Result<()> {
    if job.source.inactive && job.kind == "account" && job.resume_destination.is_none() {
        return rehome_saved(paths, job, target_home, root_target).await;
    }
    let mode = crate::store::load_config(paths)?.effective_launch_permissions();
    let permissions = start_permissions(
        mode,
        recorded_permissions(&job.source.rollout)
            .or_else(|| launch_permissions(&job.source.resume_options)),
    );
    let cwd = job
        .relocation
        .as_ref()
        .map(|r| r.to.to_string_lossy().into_owned())
        .or_else(|| {
            job.resume_directory
                .as_ref()
                .map(|d| d.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| job.source.cwd.clone());
    if let Some(target) = target.as_mut() {
        job.target_loaded = true;
        save_job(paths, job)?;
        let mut params = json!({"threadId":job.source.thread_id,"cwd":cwd,"excludeTurns":true});
        set_permissions(&mut params, permissions);
        // Native resume reconstructs the destination's index from the unchanged
        // rollout. Same-home actions use their existing native identity/index.
        if job.relocation.is_none() {
            params["path"] = json!(root_target);
        }
        if let Err(error) = target.resume_with_permissions(params).await {
            if error.to_string().starts_with("Codex rejected") {
                job.target_loaded = false;
                save_job(paths, job)?;
            }
            return Err(error);
        }
        if let Some(check) = job
            .history_checks
            .iter()
            .find(|c| c.thread_id == job.source.thread_id)
            .cloned()
        {
            phase(
                paths,
                job,
                "verifying_history",
                "Verifying the complete transferred conversation before opening it. The original is retained for recovery.",
            )?;
            super::history::verify(target, &check).await?;
        }
    }
    let config = crate::store::load_config(paths)?;
    let command = if target.is_some() {
        remote_resume_shell(
            target_home,
            &config.codex_binary,
            &job.source.resume_options,
            &job.source.thread_id,
        )
    } else {
        resume_shell(
            target_home,
            &config.codex_binary,
            &job.source.resume_options,
            &job.source.thread_id,
            root_target,
            mode,
        )
    };
    let command = format!("{command} -C {}", shell_quote(&cwd));
    phase(
        paths,
        job,
        "launching_target",
        "Starting the conversation and verifying its account and process identity.",
    )?;
    if job.source.inactive {
        super::catalog::remember(paths, &job.source)?;
        if let Some(pane) = job.resume_destination.clone() {
            // The caller created this destination without focus. A retained shell
            // must still occupy it; relaunch refuses an agent/editor replacement.
            hosting::relaunch(paths, &pane, &cwd, &command)?;
            job.source.tmux_session = crate::terminal::target(&pane)?.0.into();
            job.source.pane = pane;
        } else {
            let name = super::opening::session_name(paths, &cwd, &job.source.thread_id, None)?;
            let pane = tmux(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-s",
                &name,
                "-c",
                &cwd,
                &command,
            ])?;
            job.source.pane = pane;
            job.source.tmux_session = name;
        }
    } else {
        hosting::relaunch(paths, &job.source.pane, &cwd, &command)?;
    }
    save_job(paths, job)?;
    hosting::retain(&job.source.pane, true)?;
    confirm_runtime(paths, &job.source.pane, target_home, &job.source.thread_id).await?;
    job.source_lease_released = true;
    super::catalog::remember(paths, &job.source)?;
    super::relocation::record_location(paths, job)?;
    let mut detail = if let Some(r) = &job.relocation {
        if r.restart {
            format!(
                "Restarted Codex in {} using the installed binary. {}",
                job.source.tmux_session,
                r.backend_note.as_deref().unwrap_or(
                    "The conversation, account, working directory and permissions were preserved."
                )
            )
        } else {
            format!(
                "Conversation resumed in {}.{}",
                r.to.display(),
                r.retained_source
                    .as_ref()
                    .map(|p| format!(" Original backup retained at {}.", p.display()))
                    .unwrap_or_default()
            )
        }
    } else {
        format!(
            "Moved to {} in {}. The conversation is idle and ready to continue.",
            job.target, job.source.tmux_session
        )
    };
    if job.kind == "account" {
        if let Some(note) = super::catalog::moved(paths, job, target_home).await? {
            detail.push(' ');
            detail.push_str(&note);
        }
    }
    phase(paths, job, "complete", detail)?;
    Ok(())
}

/// Switching a stopped (saved) session only re-homes it: the verified history is already in the
/// target home and the old copy is retired. Nothing is loaded on the target app-server and no
/// terminal is opened; a later normal resume starts it there (native resume rebuilds the index
/// from the rollout file).
async fn rehome_saved(
    paths: &Paths,
    job: &mut Job,
    target_home: &Path,
    root_target: &Path,
) -> Result<()> {
    phase(
        paths,
        job,
        "verifying_history",
        "Verifying the copied history byte for byte. The session is not started.",
    )?;
    if let Ok(original) = fs::read(&job.source.rollout) {
        ensure!(
            fs::read(root_target)? == original,
            "Transferred history did not match the original; the original is retained for recovery"
        );
    }
    job.source_lease_released = true;
    save_job(paths, job)?;
    super::catalog::remember(paths, &job.source)?;
    let mut detail = format!(
        "Re-homed to {}. The session stays saved; resume it normally to start it there.",
        job.target
    );
    if let Some(note) = super::catalog::moved(paths, job, target_home).await? {
        detail.push(' ');
        detail.push_str(&note);
    }
    phase(paths, job, "complete", detail)
}

pub(super) async fn stop_source(
    paths: &Paths,
    job: &mut Job,
    mut files: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    let boundary =
        crate::paths::lock_file(&jobs_dir(paths)?.join(format!("{}.transition-lock", job.id)))?;
    boundary.lock_exclusive()?;
    ensure!(
        !cancelled(paths, job),
        "move cancelled before stopping source"
    );
    phase(
        paths,
        job,
        "stopping_source",
        "Idle state verified. Saving the conversation before stopping its Codex terminal.",
    )?;
    drop(boundary);
    if job.source.shared_server {
        let mut source = Control::connect(&job.source.home).await?;
        let ids = files
            .iter()
            .map(|file| {
                Ok(session_meta(file)?["id"]
                    .as_str()
                    .context("missing thread identity")?
                    .to_owned())
            })
            .collect::<Result<Vec<_>>>()?;
        for id in &ids {
            source.verify_unloadable(id, true).await?;
        }
        source
            .verify_unloadable(&job.source.thread_id, false)
            .await?;
        // Archival unloads only this conversation tree, leaving other sessions
        // on the account running. It also prevents an old copy being resumed by mistake.
        job.source_archived = true;
        save_job(paths, job)?;
        source
            .request("thread/archive", json!({"threadId":job.source.thread_id}))
            .await?;
        let mut archived = Vec::new();
        for id in ids {
            let thread = source.thread(&id, false).await?;
            ensure!(
                thread["status"]["type"] == "notLoaded",
                "Codex has not unloaded the archived source; history is retained for recovery"
            );
            let path = PathBuf::from(
                thread["path"]
                    .as_str()
                    .context("archived history path missing")?,
            );
            ensure!(
                !rollout_still_open(&path)?,
                "archived history still has a writer"
            );
            if id == job.source.thread_id {
                job.source.rollout = path.clone();
            }
            archived.push(path);
        }
        files = archived;
        save_job(paths, job)?;
    }
    if !job.source.inactive {
        if !job.source.shared_server {
            ensure!(
                files
                    .iter()
                    .all(|p| complete_rollout(p).unwrap_or(false) || interrupted_or_empty(p))
                    && child_processes(job.source.pid)?.is_empty(),
                "the conversation or a background task restarted; source retained, retry the move"
            );
        }
        let info = process_info(job.source.pid)?;
        ensure!(
            info.start_time == job.source.start_time,
            "source process identity changed"
        );
        if job.kind == "terminate" {
            let owned = binding_path(paths, &job.source.pane)
                .and_then(|p| Ok(serde_json::from_slice::<Binding>(&fs::read(p)?)?))
                .is_ok_and(|b| {
                    b.pid == info.pid
                        && b.start_time == info.start_time
                        && b.thread == job.source.thread_id
                        && crate::layout::same_home(&b.home, &job.source.home)
                });
            // Handoffs retain their panes for respawn. An explicit session end
            // must release that pane/name, including after a previous move.
            if owned {
                hosting::retain(&job.source.pane, false)?;
            }
        }
        stop_exact_process(info)?;
        wait_stopped(info).await?;
        for _ in 0..30 {
            if job.kind == "terminate" && pane_state(paths, &job.source.pane).is_err() {
                break;
            }
            if pane_state(paths, &job.source.pane)?.0 {
                break;
            }
            let wrapper = pane_state(paths, &job.source.pane)?.1;
            if wrapper != job.source.pid && child_processes(wrapper)?.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        ensure!(
            (job.kind == "terminate" && pane_state(paths, &job.source.pane).is_err())
                || pane_state(paths, &job.source.pane)?.0
                || (pane_state(paths, &job.source.pane)?.1 != job.source.pid
                    && child_processes(pane_state(paths, &job.source.pane)?.1)?.is_empty()),
            "tmux has not observed the stopped source yet"
        );
    }
    phase(
        paths,
        job,
        "verified",
        if job.kind == "terminate" {
            "Session stopped. Restoring its saved history to the resume list."
        } else {
            "Source saved. Copying the conversation to the prepared account."
        },
    )?;
    Ok(files)
}

pub async fn worker(paths: &Paths, id: &str) -> Result<()> {
    validate_name(id)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(jobs_dir(paths)?.join(format!("{id}.worker-lock")))?;
    lock.try_lock_exclusive()
        .context("this move already has a worker")?;
    let mut job: Job = serde_json::from_slice(&fs::read(job_path(paths, id)?)?)?;
    if let Some(mode) = job.permissions {
        crate::config::override_launch_permissions(mode);
    }
    ensure!(
        job.phase == "queued",
        "this job has already started; inspect it or use recover instead of replaying its effects"
    );
    let result = perform(paths, &mut job).await;
    if let Err(error) = &result {
        let was_cancelled =
            cancelled(paths, &job) || error.to_string().contains("confirmation declined");
        phase(
            paths,
            &mut job,
            if was_cancelled { "cancelled" } else { "failed" },
            format!("{error:#}"),
        )?;
        // Cancelled or failed with the source still running: nothing was stopped, so the
        // stop gate recorded at admission must not outlive the job (contract 1.40.1).
        if super::source_still_running(&job) {
            let _ = super::catalog::clear_stop_intent_if_matches(
                paths,
                &job.source.thread_id,
                Some(&job.id),
            );
        }
        if job.source_archived
            || job.target_loaded
            || !job.prepared_threads.is_empty()
            || job
                .relocation
                .as_ref()
                .is_some_and(|r| r.state != "prepared")
            || (!job.source.inactive
                && pane_state(paths, &job.source.pane).is_ok_and(|(dead, _)| dead))
        {
            if let Err(recovery_error) = recover_inner(paths, id).await {
                // Recovery can persist partial progress before failing. Keep
                // those flags instead of overwriting them with the worker's
                // older in-memory journal.
                let mut retained: Job = serde_json::from_slice(&fs::read(job_path(paths, id)?)?)?;
                retained.detail = format!(
                    "{}. Automatic recovery could not finish: {recovery_error:#}. Use Recover in Activity to continue restoring the saved conversation.",
                    retained.detail
                );
                save_job(paths, &retained)?;
            }
        }
    }
    // The move is already committed. A continuation failure must never roll
    // back or stop the destination, where this request may already be running.
    if result.is_ok() && job.phase == "complete" && job.resume_after_move {
        if let Err(error) = continue_work(paths, &mut job).await {
            if job.continuation_delivery.is_empty() {
                job.continuation_delivery = "not_sent".into();
            }
            job.detail = format!(
                "Move completed. Automatic continuation could not be confirmed: {error}. The destination remains open. Check it before sending another message; this request will not be retried automatically."
            );
            save_job(paths, &job)?;
        }
    }
    // The job is done: leave the cached snapshots showing its result (1.45.1).
    crate::api::refresh_active_snapshot(paths).await;
    result
}

async fn continue_work(paths: &Paths, job: &mut Job) -> Result<()> {
    if job.kind != "account" || !job.continuation_delivery.is_empty() {
        return Ok(());
    }
    let _lock = super::catalog::operation_lock(paths, &job.source.thread_id)?;
    ensure!(
        !jobs(paths)?.iter().any(|other| other.id != job.id && job_conflicts_with_source(paths, other, &job.source)),
        "another session operation is already queued"
    );
    let (dead, pid) = pane_state(paths, &job.source.pane)?;
    ensure!(
        !dead
            && process_home(pid)? == paths.account(&job.target)?
            && resumed_thread(pid)?.as_deref() == Some(&job.source.thread_id),
        "the destination pane changed before continuation"
    );
    let mut target = Control::connect(&paths.account(&job.target)?).await?;
    let thread = target.thread(&job.source.thread_id, false).await?;
    let queue = target
        .request(
            "thread/queue/list",
            json!({"threadId":job.source.thread_id,"limit":1}),
        )
        .await?;
    if thread["status"]["type"] != "idle"
        || !queue["data"]
            .as_array()
            .is_some_and(|items| items.is_empty())
    {
        job.continuation_delivery = "skipped".into();
        job.detail = "Move completed. The destination is already working or has pending input; no extra continuation message was sent.".into();
        return save_job(paths, job);
    }
    job.continuation_delivery = "sending".into();
    job.detail = "Move completed. Sending the automatic continuation message.".into();
    save_job(paths, job)?;
    let message = format!(
        "[codex-switcher continue {}] The requested account switch completed successfully. Pick up where you left off using your handoff summary and existing conversation. Resume work and any goal paused for this move. Review paused background tasks and subagents before restarting them, and do not repeat completed actions.",
        job.id
    );
    match target
        .request(
            "turn/start",
            json!({"threadId":job.source.thread_id,"clientUserMessageId":job.id,"input":[{"type":"text","text":message}]}),
        )
        .await
    {
        Ok(ack) => {
            let turn = ack["turn"]["id"].as_str().filter(|id| !id.is_empty() && id.len()<=256 && !id.chars().any(char::is_control));
            if turn.is_none() || ack["turn"]["status"] != "inProgress" {
                job.continuation_delivery = "unconfirmed".into();
                bail!("Codex returned no valid continuation acknowledgment; delivery is unknown");
            }
            job.continuation_turn_id = turn.map(str::to_owned);
            job.continuation_delivery = "accepted".into();
            job.continuation_observation = "unknown".into();
            job.detail = "Move completed. Codex accepted the continuation request; execution is not yet verified. It will not be resent automatically.".into();
            save_job(paths, job)?;
            // One bounded public-summary observation, never a reply scrape or resend.
            if let Ok(page) = target.request("thread/turns/list",json!({"threadId":job.source.thread_id,"itemsView":"summary","sortDirection":"desc","limit":1})).await {
                if let Some(state) = continuation_observation(&page, &job.id, job.continuation_turn_id.as_deref().unwrap()) {
                    job.continuation_observation = state.into();
                    job.detail = format!("Move completed. Codex accepted the continuation; correlated public input observed ({state}). No automatic replay.");
                }
            }
            save_job(paths, job)
        }
        Err(error) => {
            job.continuation_delivery = "unconfirmed".into();
            Err(error)
        }
    }
}

fn continuation_observation<'a>(page: &'a Value, client: &str, turn: &str) -> Option<&'a str> {
    let turns = page["data"].as_array()?;
    if turns.len() != 1 || turns[0]["id"] != turn || turns[0]["itemsView"] != "summary" {
        return None;
    }
    let items = turns[0]["items"].as_array()?;
    if items.len() > 2
        || items
            .iter()
            .filter(|item| item["type"] == "userMessage")
            .count()
            != 1
        || !items
            .iter()
            .any(|item| item["type"] == "userMessage" && item["clientId"] == client)
        || items
            .iter()
            .any(|item| item["type"] != "userMessage" && item["type"] != "agentMessage")
    {
        return None;
    }
    match turns[0]["status"].as_str()? {
        "inProgress" => Some("in_progress"),
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "interrupted" => Some("interrupted"),
        _ => None,
    }
}

pub async fn recover(paths: &Paths, id: &str) -> Result<Job> {
    let lock = crate::paths::lock_file(
        &jobs_dir(paths)?.join(format!("{}.worker-lock", validate_job_id(id)?)),
    )?;
    lock.try_lock_exclusive()
        .context("the move worker is still reconciling this job; retry when it stops")?;
    recover_inner(paths, id).await
}
fn validate_job_id(id: &str) -> Result<&str> {
    validate_name(id)?;
    Ok(id)
}
async fn recover_inner(paths: &Paths, id: &str) -> Result<Job> {
    let mut job: Job = serde_json::from_slice(&fs::read(job_path(paths, id)?)?)?;
    if matches!(job.kind.as_str(), "terminate" | "pause") {
        return super::session_control::recover(paths, job).await;
    }
    if job.relocation.is_some() {
        return super::relocation::recover(paths, job).await;
    }
    ensure!(
        job.phase == "failed"
            || (!matches!(job.phase.as_str(), "complete" | "cancelled" | "recovered")
                && !handoff_worker_alive(id)
                && daemon::now() - job.created_at > 10),
        "only failed or interrupted moves need recovery"
    );
    if job.target_loaded || !job.prepared_threads.is_empty() {
        let target_home = paths.account(&job.target)?;
        let mut target = Control::connect(&target_home).await?;
        let mut ids = job.prepared_threads.clone();
        if job.target_loaded {
            ids.push(job.source.thread_id.clone());
        }
        for id in ids {
            let thread = target.thread(&id, false).await?;
            if thread["status"]["type"] != "notLoaded" {
                target.verify_unloadable(&id, false).await?;
                ensure!(
                    pane_state(paths, &job.source.pane).map_or(true, |(dead, _)| dead),
                    "a destination terminal may still be attached; close it before recovery"
                );
                target
                    .request("thread/archive", json!({"threadId":id}))
                    .await?;
            }
        }
        job.target_loaded = false;
        job.prepared_threads.clear();
        save_job(paths, &job)?;
    }
    restore_source(paths, job).await
}

pub(super) async fn restore_source(paths: &Paths, mut job: Job) -> Result<Job> {
    let failure = job.detail.clone();
    if job.source_archived {
        let mut control = Control::connect(&job.source.home).await?;
        let mut thread = control.thread(&job.source.thread_id, false).await?;
        if thread["path"].as_str().is_some_and(|path| {
            Path::new(path).starts_with(job.source.home.join("archived_sessions"))
        }) {
            control
                .request("thread/unarchive", json!({"threadId":job.source.thread_id}))
                .await?;
            thread = control.thread(&job.source.thread_id, false).await?;
        }
        job.source.rollout = PathBuf::from(
            thread["path"]
                .as_str()
                .context("source history path missing")?,
        );
        ensure!(
            job.source
                .rollout
                .starts_with(job.source.home.join("sessions"))
                && job.source.rollout.is_file(),
            "source history is not yet restored; retry recovery"
        );
        job.source_archived = false;
        save_job(paths, &job)?;
    }
    if job.source.inactive || !pane_state(paths, &job.source.pane)?.0 {
        phase(
            paths,
            &mut job,
            "recovered",
            format!("{failure}. The original history was restored; select the source to retry."),
        )?;
        return Ok(job);
    }
    let mode = crate::store::load_config(paths)?.effective_launch_permissions();
    let command = if job.source.shared_server {
        let mut params =
            json!({"threadId":job.source.thread_id,"cwd":job.source.cwd,"excludeTurns":true});
        set_permissions(
            &mut params,
            start_permissions(
                mode,
                recorded_permissions(&job.source.rollout)
                    .or_else(|| launch_permissions(&job.source.resume_options)),
            ),
        );
        Control::connect(&job.source.home)
            .await?
            .resume_with_permissions(params)
            .await?;
        remote_resume_shell(
            &job.source.home,
            &job.source.binary,
            &job.source.resume_options,
            &job.source.thread_id,
        )
    } else {
        resume_shell(
            &job.source.home,
            &job.source.binary,
            &job.source.resume_options,
            &job.source.thread_id,
            &job.source.rollout,
            mode,
        )
    };
    hosting::relaunch(paths, &job.source.pane, &job.source.cwd, &command)?;
    confirm_runtime(
        paths,
        &job.source.pane,
        &job.source.home,
        &job.source.thread_id,
    )
    .await?;
    phase(
        paths,
        &mut job,
        "recovered",
        format!("{failure}. The original conversation was reopened on its source account."),
    )?;
    Ok(job)
}

pub(super) fn remote_resume_shell(
    home: &Path,
    binary: &str,
    options: &[String],
    thread: &str,
) -> String {
    // Permissions are already set by thread/start or thread/resume. Native
    // remote TUIs reject permission overrides rather than applying them.
    let options = remote_tui_options(options)
        .iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = format!(
        "exec env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN CODEX_HOME={} {} resume {} {}",
        shell_quote(&home.to_string_lossy()),
        shell_quote(binary),
        options,
        shell_quote(thread)
    );
    command.push_str(" --remote ");
    command.push_str(&shell_quote(&format!(
        "unix://{}",
        home.join("app-server-control/app-server-control.sock")
            .display()
    )));
    command
}

async fn confirm_runtime(paths: &Paths, pane: &str, home: &Path, thread: &str) -> Result<u32> {
    let mut verified_pid = None;
    for _ in 0..50 {
        let (dead, pid) = pane_state(paths, pane)?;
        if dead && pane.starts_with("herdr/") {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        if dead {
            bail!(
                "destination Codex exited during startup; source history is available for recovery"
            );
        }
        if process_home(pid).is_ok_and(|actual| crate::layout::same_home(&actual, home))
            && resumed_thread(pid)?.as_deref() == Some(thread)
        {
            verified_pid = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let pid = verified_pid
        .context("destination did not confirm the expected account and conversation")?;
    let identity = process_info(pid)?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    ensure!(
        !pane_state(paths, pane)?.0
            && process_info(pid)
                .is_ok_and(|p| p.start_time == identity.start_time && p.state != 'Z'),
        "destination exited during startup; source history is available for recovery"
    );
    bind_pane(paths, pane, pid, home, thread)?;
    Ok(pid)
}

#[cfg(test)]
mod continuation_tests {
    use super::*;
    #[test]
    fn public_evidence_requires_exact_client_turn_and_summary() {
        let mut page = json!({"data":[{"id":"turn","status":"inProgress","itemsView":"summary","items":[{"type":"userMessage","clientId":"job"}]}]});
        assert_eq!(
            continuation_observation(&page, "job", "turn"),
            Some("in_progress")
        );
        assert_eq!(continuation_observation(&page, "other", "turn"), None);
        assert_eq!(continuation_observation(&page, "job", "old"), None);
        page["data"][0]["status"] = json!("failed");
        assert_eq!(
            continuation_observation(&page, "job", "turn"),
            Some("failed")
        );
        page["data"][0]["itemsView"] = json!("full");
        assert_eq!(continuation_observation(&page, "job", "turn"), None);
    }
}

#[cfg(test)]
mod legacy_path_tests {
    use super::*;

    /// Codex's `state_5.sqlite` keeps rollout paths under the legacy home path,
    /// which `layout migrate` left as a symlink to the new home.
    #[test]
    fn reported_history_path_accepts_the_legacy_symlink_spelling() {
        let d = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(d.path()).unwrap();
        let new = root.join("accounts/codex/personal");
        fs::create_dir_all(new.join("sessions/2026/10/29")).unwrap();
        fs::create_dir_all(root.join("accounts")).unwrap();
        std::os::unix::fs::symlink("codex/personal", root.join("accounts/personal")).unwrap();
        let rollout = "sessions/2026/10/29/rollout-x.jsonl";
        fs::write(new.join(rollout), "").unwrap();
        let legacy = root.join("accounts/personal").join(rollout);
        // Existing and not-yet-existing files, legacy and canonical spellings.
        for reported in [
            legacy.clone(),
            new.join(rollout),
            new.join("sessions/a/new.jsonl"),
        ] {
            let got = reported_history_path(reported.to_str().unwrap(), &new).unwrap();
            assert!(got.starts_with(&new), "{got:?}");
        }
        assert!(
            reported_history_path(legacy.to_str().unwrap(), &root.join("accounts/codex/work"))
                .is_err()
        );
        let outside = root.join("accounts/personal/auth.json");
        assert!(reported_history_path(outside.to_str().unwrap(), &new).is_err());
        // The source side of a transfer may also be spelled through the symlink.
        let source = root.join("accounts/personal").join(rollout);
        let target = target_path(&source, &new, &root.join("accounts/codex/work")).unwrap();
        assert_eq!(target, root.join("accounts/codex/work").join(rollout));
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::*;

    fn job(kind: &str, phase: &str, created_at: i64, thread: &str) -> Job {
        serde_json::from_value(json!({
            "id": "stop-1", "kind": kind, "target": "", "phase": phase, "detail": "",
            "created_at": created_at,
            "source": {"account":"personal","pane":"%1","tmux_session":"fixture","pid":1,
                "cwd":"/fixture","thread_id":thread,"rollout":"/fixture/rollout",
                "home":"/fixture","binary":"fake","resume_options":[]}
        }))
        .unwrap()
    }

    #[test]
    fn a_cancelled_or_failed_stop_job_does_not_leave_a_stale_gate_on_a_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("state"),
            runtime: dir.path().join("runtime"),
        };
        let thread = uuid::Uuid::new_v4().to_string();
        let me = std::process::id();
        let start = process_info(me).unwrap().start_time;
        let gate = |phase: &str, pid: u32, start_time: u64, thread_id: &str| {
            let mut j = job("terminate", phase, daemon::now(), thread_id);
            j.id = uuid::Uuid::new_v4().to_string();
            j.source.pid = pid;
            j.source.start_time = start_time;
            save_job(&paths, &j).unwrap();
            super::catalog::set_stop_intent(&paths, &thread, Some(j.id.clone())).unwrap();
            j.id
        };
        let stop = |id: &str| {
            super::messaging_stop_intent(&paths, &thread)
                .unwrap()
                .as_deref()
                == Some(id)
        };
        // A finished stop keeps its gate: the session really was stopped.
        let done = gate("complete", me, start, &thread);
        assert!(stop(&done));
        // Cancelled, but the process it was going to stop is still the same one: stale.
        gate("cancelled", me, start, &thread);
        assert!(
            super::messaging_stop_intent(&paths, &thread)
                .unwrap()
                .is_none()
        );
        assert!(
            super::catalog::stop_intent(&paths, &thread)
                .unwrap()
                .is_none(),
            "healed in the catalog"
        );
        gate("failed", me, start, &thread);
        assert!(
            super::messaging_stop_intent(&paths, &thread)
                .unwrap()
                .is_none()
        );
        // The process is gone or was replaced (other start time): the gate stands.
        let replaced = gate("cancelled", me, start + 1, &thread);
        assert!(stop(&replaced));
        // A job that names another conversation never heals this one.
        let other = gate("cancelled", me, start, &uuid::Uuid::new_v4().to_string());
        assert!(stop(&other));
    }

    #[tokio::test]
    async fn a_stop_waiting_for_idle_expires_and_releases_its_stop_gate() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            root: dir.path().join("state"),
            runtime: dir.path().join("runtime"),
        };
        let thread = uuid::Uuid::new_v4().to_string();
        let now = daemon::now();
        // A stop whose client was interrupted: the worker still waits for idle.
        let old = job(
            "terminate",
            "waiting_for_idle",
            now - TERMINATE_WAIT_SECONDS - 5,
            &thread,
        );
        assert_eq!(old.expires_at(), Some(now - 5));
        super::catalog::set_stop_intent(&paths, &thread, Some("stop-1".into())).unwrap();
        let error = tick(&paths, &old).await.unwrap_err().to_string();
        assert!(error.contains("expired") && error.contains("nothing was stopped"));
        // Its stop gate went with it, so sends are not refused for a stop that never ran.
        assert!(
            super::catalog::stop_intent(&paths, &thread)
                .unwrap()
                .is_none()
        );

        // A newer stop's gate is never released by an older request's expiry.
        super::catalog::set_stop_intent(&paths, &thread, Some("stop-2".into())).unwrap();
        assert!(tick(&paths, &old).await.is_err());
        assert_eq!(
            super::catalog::stop_intent(&paths, &thread)
                .unwrap()
                .as_deref(),
            Some("stop-2")
        );

        // Within the window, other kinds and non-waiting phases keep going.
        for waiting in [
            job("terminate", "waiting_for_idle", now, &thread),
            job(
                "account",
                "waiting_for_idle",
                now - 10 * TERMINATE_WAIT_SECONDS,
                &thread,
            ),
            job(
                "terminate",
                "stopping_source",
                now - 10 * TERMINATE_WAIT_SECONDS,
                &thread,
            ),
        ] {
            tick(&paths, &waiting).await.unwrap();
        }
        // A journal that predates `expires_at` gets the default window from its creation.
        let mut explicit = job("terminate", "waiting_for_idle", now, &thread);
        explicit.expires_at = now + 7;
        assert_eq!(explicit.expires_at(), Some(now + 7));
    }
}
