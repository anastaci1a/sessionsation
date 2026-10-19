//! Resume only the still-open session that supplied the usage-stop evidence.
use super::*;
use crate::{
    autostart::{Policy, Waiting},
    control::Control,
};

pub async fn latest_turn(control: &mut Control, id: &str, snapshot: &Value) -> Result<Value> {
    if snapshot["historyMode"] == "paginated" {
        let page = control
            .request(
                "thread/turns/list",
                json!({"threadId":id,"limit":1,"sortDirection":"desc"}),
            )
            .await?;
        return page["data"]
            .as_array()
            .and_then(|a| a.first())
            .cloned()
            .context("no latest native turn");
    }
    control.thread(id, true).await?["turns"]
        .as_array()
        .and_then(|a| a.last())
        .cloned()
        .context("no latest native turn")
}

pub async fn continue_after_reset(
    paths: &Paths,
    source: &Instance,
    policy: &Policy,
    waiting: &Waiting,
) -> Result<()> {
    let _lock = catalog::operation_lock(paths, &source.thread_id)?;
    catalog::ensure_available(paths, &source.thread_id)?;
    ensure!(
        !jobs(paths)?
            .iter()
            .any(|j| job_conflicts_with_source(paths, j, source)
                || (j.source.thread_id == source.thread_id && handoff_worker_alive(&j.id))),
        "another session operation is running"
    );
    let info = process_info(source.pid)?;
    let (dead, pid) = pane_state(paths, &source.pane)?;
    ensure!(
        !dead
            && info.start_time == waiting.start_time
            && info.state != 'Z'
            && (pid == source.pid
                || child_processes(pid)?
                    .iter()
                    .any(|p| p.pid == source.pid && p.start_time == source.start_time)),
        "session process changed"
    );
    let mut control = Control::connect(&source.home).await?;
    for file in descendants(&source.home, &source.thread_id)? {
        let meta = session_meta(&file)?;
        let id = meta["id"].as_str().context("missing child identity")?;
        control
            .verify_unloadable(id, id != source.thread_id)
            .await?;
    }
    let snapshot = control.thread(&source.thread_id, false).await?;
    let turn = latest_turn(&mut control, &source.thread_id, &snapshot).await?;
    ensure!(
        turn["id"].as_str() == Some(&waiting.turn),
        "new input arrived before continuation"
    );
    ensure!(
        matches!(
            turn["status"].as_str(),
            Some("completed" | "failed" | "interrupted")
        ),
        "turn has not ended"
    );
    let reserved = daemon::call(
        paths,
        "auto-start/reserve",
        json!({"thread":source.thread_id,"revision":policy.revision,"event":waiting.event}),
    )
    .await?;
    let id = reserved["policy"]["sending"]
        .as_str()
        .context("missing continuation reservation")?;
    // The journal is durable before the request. Even a lost reply is consumed
    // once and disables automation; no automatic replay or process restart.
    let message = format!(
        "[codex-switcher usage-reset {id}] Usage capacity has reset after your work was stopped by a usage limit. Continue the unfinished work from this conversation and any usage-pause summary. Do not repeat completed actions. Review the state of paused goals, subagents and background tasks before resuming them. If the task was already completed or the user asked you to stop, remain idle."
    );
    let result = control
        .request(
            "turn/start",
            json!({"threadId":source.thread_id,"input":[{"type":"text","text":message}]}),
        )
        .await;
    daemon::call(
        paths,
        "auto-start/finish",
        json!({"thread":source.thread_id,"reservation":id,"accepted":result.as_ref().is_ok_and(|v|v["turn"]["id"].as_str().is_some_and(|id|!id.is_empty()))}),
    )
    .await?;
    Ok(())
}
