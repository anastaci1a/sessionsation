//! Pause or end a live session without transferring or reopening its history.
use super::movement::{identify, phase, risks, stop_source, wait_idle};
use super::*;
use crate::control::Control;

pub async fn queue_termination(paths: &Paths, pane: &str, options: MoveOptions) -> Result<Job> {
    queue(paths, pane, options, "terminate").await
}
pub async fn queue_pause(paths: &Paths, pane: &str, options: MoveOptions) -> Result<Job> {
    queue(paths, pane, options, "pause").await
}
async fn queue(paths: &Paths, pane: &str, options: MoveOptions, kind: &str) -> Result<Job> {
    let source = live_sessions(paths)
        .await?
        .into_iter()
        .find(|i| !i.inactive && i.pane == pane)
        .context(
            "selected Codex session is no longer active; saved conversations are already stopped",
        )?;
    let account = source.account.clone();
    // A manual pause/end expresses the user's intent to stay stopped. Cancel
    // auto-start even if the monitor has not observed the quota failure yet.
    let state = crate::store::load_state_snapshot(paths)?;
    if state
        .automatic_start
        .get(&source.thread_id)
        .is_some_and(|p| p.enabled || p.waiting.is_some())
    {
        daemon::call(
            paths,
            "auto-start/disable",
            json!({"thread":source.thread_id}),
        )
        .await?;
    }
    enqueue(paths, source, &account, options, None, kind)
}

pub(super) async fn perform(paths: &Paths, job: &mut Job) -> Result<()> {
    identify(paths, job).await?;
    if !job.source.warnings.is_empty() {
        risks(paths, job, job.source.warnings.join(" ")).await?;
    }
    let files = wait_idle(paths, job).await?;
    if job.kind == "pause" {
        return phase(
            paths,
            job,
            "complete",
            "Work paused and background terminals stopped. The session remains open; no new turn was started.",
        );
    }
    job.termination_threads = files
        .iter()
        .map(|file| {
            Ok(session_meta(file)?["id"]
                .as_str()
                .context("missing thread identity")?
                .to_owned())
        })
        .collect::<Result<_>>()?;
    save_job(paths, job)?;
    stop_source(paths, job, files).await?;
    restore_histories(paths, job).await?;
    phase(
        paths,
        job,
        "complete",
        format!(
            "Session ended. Conversation {} remains saved on {} and can be resumed. No conversation or project files were deleted.",
            job.source.thread_id,
            job.source
                .display_account
                .as_deref()
                .unwrap_or(&job.source.account)
        ),
    )
}

async fn restore_histories(paths: &Paths, job: &mut Job) -> Result<()> {
    if !job.source_archived {
        return Ok(());
    }
    let mut control = Control::connect(&job.source.home).await?;
    ensure!(
        !job.termination_threads.is_empty(),
        "missing termination journal; history retained for inspection"
    );
    // Read before each request: recovery after an uncertain reply must not repeat
    // unarchive blindly, nor load/stop a conversation the user has already resumed.
    for id in job.termination_threads.clone() {
        let mut thread = control.thread(&id, false).await?;
        let mut path = PathBuf::from(
            thread["path"]
                .as_str()
                .context("saved history path missing")?,
        );
        if path.starts_with(job.source.home.join("archived_sessions")) {
            control
                .request("thread/unarchive", json!({"threadId":id}))
                .await?;
            thread = control.thread(&id, false).await?;
            path = PathBuf::from(
                thread["path"]
                    .as_str()
                    .context("unarchived history path missing")?,
            );
        }
        ensure!(
            path.starts_with(job.source.home.join("sessions")) && path.is_file(),
            "conversation is not yet back in the resume list; retry recover"
        );
        if id == job.source.thread_id {
            job.source.rollout = path;
        }
    }
    job.source_archived = false;
    save_job(paths, job)
}

pub(super) async fn recover(paths: &Paths, mut job: Job) -> Result<Job> {
    ensure!(
        job.phase == "failed"
            || (!matches!(job.phase.as_str(), "complete" | "cancelled" | "recovered")
                && !handoff_worker_alive(&job.id)
                && daemon::now() - job.created_at > 10),
        "only failed or interrupted session endings need recovery"
    );
    let failure = job.detail.clone();
    if job.kind == "pause" {
        phase(
            paths,
            &mut job,
            "recovered",
            format!(
                "{failure}. No session was stopped or relaunched by recovery. Inspect its activity and retry pausing if needed."
            ),
        )?;
        return Ok(job);
    }
    restore_histories(paths, &mut job).await?;
    let stopped = process_info(job.source.pid).map_or(true, |p| {
        p.start_time != job.source.start_time || p.state == 'Z'
    });
    phase(
        paths,
        &mut job,
        if stopped { "complete" } else { "recovered" },
        format!(
            "{failure}. Saved history is available for resume. {} No session was relaunched by recovery.",
            if stopped {
                "The original process has stopped."
            } else {
                "The original process is still running; select it again to retry."
            }
        ),
    )?;
    Ok(job)
}
