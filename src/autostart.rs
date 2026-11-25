//! Opt-in continuation after an observed usage stop and a verified reset.
//! The daemon owns the journal; a sending reservation is never replayed.
#[cfg(test)]
mod tests;
use crate::{
    config::Config,
    daemon, external,
    model::{Account, State, Window},
    paths::Paths,
    store,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const WARNING: &str = "Experimental: automatically sends one continuation when a usage reset restores capacity after a confirmed usage failure or auto-switch exhaustion pause. Normally completed or idle work stays idle. Keep the session open; closing it, changing accounts, new input or another session operation cancels the pending restart. Disable with x in Session actions or d here. Already-submitted requests cannot be undone. Uncertain delivery disables auto-start for review.";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub enabled: bool,
    pub revision: u64,
    pub waiting: Option<Waiting>,
    pub last_event: Option<String>,
    pub sending: Option<String>,
    pub detail: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Waiting {
    pub event: String,
    pub account: String,
    pub turn: String,
    pub pane: String,
    pub pid: u32,
    pub start_time: u64,
    pub observed_at: i64,
    pub windows: Vec<Window>,
    pub threshold_pause: bool,
    pub operations: Vec<String>,
}

pub fn request(state: &mut State, config: &Config, method: &str, params: &Value) -> Result<Value> {
    let thread = params["thread"].as_str().context("missing thread UUID")?;
    uuid::Uuid::parse_str(thread)?;
    let mut policy = state
        .automatic_start
        .get(thread)
        .cloned()
        .unwrap_or_default();
    match method {
        "auto-start/get" => {}
        "auto-start/enable" => {
            ensure!(
                params["accept_experimental"] == true,
                "explicit experimental consent required: {WARNING}"
            );
            ensure!(
                policy.sending.is_none(),
                "a continuation is already being submitted"
            );
            policy.enabled = true;
            policy.revision += 1;
            policy.detail = "Watching for a usage stop; idle work will stay idle.".into();
        }
        "auto-start/disable" => {
            policy.enabled = false;
            policy.waiting = None;
            policy.revision += 1;
            policy.detail =
                "Auto-start disabled. A submitted continuation cannot be undone.".into();
        }
        "auto-start/observe" => {
            ensure!(
                policy.enabled && params["revision"].as_u64() == Some(policy.revision),
                "auto-start preferences changed"
            );
            let waiting: Waiting = serde_json::from_value(params["waiting"].clone())?;
            ensure!(
                policy.sending.is_none() && policy.waiting.is_none(),
                "already waiting or submitting"
            );
            ensure!(
                policy.last_event.as_deref() != Some(&waiting.event),
                "usage stop already handled"
            );
            ensure!(
                state.accounts.contains_key(&waiting.account),
                "account is no longer enrolled"
            );
            policy.last_event = Some(waiting.event.clone());
            policy.waiting = Some(waiting);
            policy.detail =
                "Usage stopped unfinished work. Waiting for a fresh reading after reset.".into();
        }
        "auto-start/note" => {
            ensure!(
                params["revision"].as_u64() == Some(policy.revision),
                "auto-start preferences changed"
            );
            policy.detail = params["detail"].as_str().context("missing detail")?.into();
        }
        "auto-start/clear" => {
            ensure!(
                params["revision"].as_u64() == Some(policy.revision),
                "auto-start preferences changed"
            );
            policy.waiting = None;
            policy.detail = params["detail"]
                .as_str()
                .unwrap_or("Pending auto-start cancelled; watching for a new usage stop.")
                .into();
        }
        "auto-start/reserve" => {
            ensure!(
                policy.enabled && params["revision"].as_u64() == Some(policy.revision),
                "auto-start disabled or preferences changed"
            );
            ensure!(policy.sending.is_none(), "continuation already reserved");
            let waiting = policy.waiting.as_ref().context("no usage stop recorded")?;
            ensure!(
                params["event"].as_str() == Some(&waiting.event),
                "usage stop changed"
            );
            let account = state
                .accounts
                .get(&waiting.account)
                .context("account removed")?;
            ensure!(
                reset_ready(waiting, account, config, daemon::now()),
                "usage reset is not verified"
            );
            policy.sending = Some(uuid::Uuid::new_v4().to_string());
            policy.detail = "Submitting the continuation after verified usage reset.".into();
        }
        "auto-start/finish" => {
            ensure!(
                policy.sending.as_deref() == params["reservation"].as_str()
                    && policy.sending.is_some(),
                "reservation changed"
            );
            policy.sending = None;
            policy.waiting = None;
            if params["accepted"] == true {
                policy.detail = "Codex accepted the continuation after usage reset.".into();
            } else {
                policy.enabled = false;
                policy.revision += 1;
                policy.detail="Continuation delivery is unconfirmed. Auto-start disabled; inspect the session before trying again. It will not be replayed.".into();
            }
        }
        _ => anyhow::bail!("unknown auto-start method"),
    }
    let result = json!({"policy":policy,"active":policy.enabled,"warning":WARNING});
    if method != "auto-start/get" {
        state.automatic_start.insert(thread.into(), policy);
    }
    Ok(result)
}

pub fn reconcile(state: &mut State) {
    for policy in state.automatic_start.values_mut() {
        if policy.sending.take().is_some() {
            policy.enabled = false;
            policy.waiting = None;
            policy.revision += 1;
            policy.detail="Supervisor restarted during an uncertain continuation. Auto-start disabled; inspect the session. No request was replayed.".into();
        }
    }
}

pub fn reset_ready(waiting: &Waiting, account: &Account, config: &Config, now: i64) -> bool {
    let usage = &account.usage;
    if !account.enabled
        || account.auth_required
        || !usage.fresh(now, config.stale_seconds)
        || usage.observed_at <= waiting.observed_at
        || usage.exhausted
        || usage.ordinary_usage_allowed == Some(false)
        || usage.spend_control_reached == Some(true)
        || (waiting.threshold_pause && account.switch_thresholds.needs_plan_review)
    {
        return false;
    }
    let current: Vec<_> = [&usage.short, &usage.weekly]
        .into_iter()
        .flatten()
        .collect();
    if current.iter().any(|w| {
        w.used_percent
            >= if waiting.threshold_pause {
                if w.duration_minutes == 300 {
                    account.switch_thresholds.short_percent
                } else {
                    account.switch_thresholds.weekly_percent
                }
            } else {
                100.
            }
    }) {
        return false;
    }
    !waiting.windows.is_empty()
        && waiting.windows.iter().all(|old| {
            old.resets_at <= now
                && current.iter().any(|w| {
                    w.duration_minutes == old.duration_minutes && w.resets_at > old.resets_at
                })
        })
}

/// Run serially with rotation, using the same per-conversation operation lock.
pub async fn tick(paths: &Paths) -> Result<()> {
    let state = store::load_state_snapshot(paths)?;
    if !state
        .automatic_start
        .values()
        .any(|p| p.enabled || p.sending.is_some())
    {
        return Ok(());
    }
    let config = store::load_config(paths)?;
    let sources = external::live_sessions(paths).await?;
    let jobs = external::jobs(paths)?;
    for (thread, policy) in &state.automatic_start {
        if let Some(id) = &policy.sending {
            // Only one monitor exists. A reservation left between ticks had an
            // uncertain finish response; conservatively disable without replay.
            daemon::call(
                paths,
                "auto-start/finish",
                json!({"thread":thread,"reservation":id,"accepted":false}),
            )
            .await?;
            continue;
        }
        if !policy.enabled {
            continue;
        }
        let live: Vec<_> = sources
            .iter()
            .filter(|s| s.thread_id == *thread && !s.inactive && !s.tmux_dead && s.shared_server)
            .collect();
        let related: Vec<_> = jobs
            .iter()
            .filter(|j| j.source.thread_id == *thread)
            .collect();
        if related.iter().any(|j| {
            !matches!(
                j.phase.as_str(),
                "complete" | "failed" | "cancelled" | "recovered"
            ) || external::handoff_worker_alive(&j.id)
        }) {
            continue;
        }
        if live.len() != 1 {
            if policy.waiting.is_some() {
                clear(
                    paths,
                    thread,
                    policy,
                    "Session closed or ambiguous; pending auto-start cancelled.",
                )
                .await?;
            }
            continue;
        }
        let source = live[0];
        let Some(account) = state.accounts.get(&source.account) else {
            continue;
        };
        let result = inspect(
            paths, &state, &config, thread, policy, source, account, &related,
        )
        .await;
        // Read/probe failures never imply that the session is idle or reset.
        if let Err(error) = result {
            let detail = format!("Auto-start waiting: {error}");
            if policy.detail != detail {
                let _ = daemon::call(
                    paths,
                    "auto-start/note",
                    json!({"thread":thread,"revision":policy.revision,"detail":detail}),
                )
                .await;
            }
        }
    }
    Ok(())
}
async fn clear(paths: &Paths, thread: &str, policy: &Policy, detail: &str) -> Result<()> {
    daemon::call(
        paths,
        "auto-start/clear",
        json!({"thread":thread,"revision":policy.revision,"detail":detail}),
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn inspect(
    paths: &Paths,
    state: &State,
    config: &Config,
    thread: &str,
    policy: &Policy,
    source: &external::Instance,
    account: &Account,
    jobs: &[&external::Job],
) -> Result<()> {
    let mut control = crate::control::Control::connect(&source.home).await?;
    let snapshot = control.thread(thread, false).await?;
    if !matches!(
        snapshot["status"]["type"].as_str(),
        Some("idle" | "systemError")
    ) {
        if policy.waiting.is_some() {
            clear(
                paths,
                thread,
                policy,
                "Session resumed or changed; pending auto-start cancelled.",
            )
            .await?;
        }
        return Ok(());
    }
    let turn = external::autostart::latest_turn(&mut control, thread, &snapshot).await?;
    let turn_id = turn["id"]
        .as_str()
        .context("missing latest turn identity")?;
    let latest_job = jobs
        .iter()
        .max_by_key(|j| (j.created_at, &j.id))
        .map(|j| j.id.clone());
    let mut operations: Vec<_> = jobs.iter().map(|j| j.id.clone()).collect();
    operations.sort();
    if let Some(waiting) = &policy.waiting {
        if waiting.account != source.account
            || waiting.pane != source.pane
            || waiting.pid != source.pid
            || waiting.start_time != source.start_time
            || waiting.turn != turn_id
            || waiting.operations != operations
        {
            return clear(
                paths,
                thread,
                policy,
                "New input, account, process or session operation; pending auto-start cancelled.",
            )
            .await;
        }
        let queue = control
            .request("thread/queue/list", json!({"threadId":thread,"limit":1}))
            .await?;
        if queue["data"].as_array().is_some_and(|q| !q.is_empty()) {
            return clear(
                paths,
                thread,
                policy,
                "New input queued; pending auto-start cancelled.",
            )
            .await;
        }
        if !reset_ready(waiting, account, config, daemon::now()) {
            return Ok(());
        }
        external::autostart::continue_after_reset(paths, source, policy, waiting).await?;
        return Ok(());
    }
    if !matches!(
        turn["status"].as_str(),
        Some("completed" | "failed" | "interrupted")
    ) {
        return Ok(());
    }
    let quota =
        turn["status"] == "failed" && turn["error"]["codexErrorInfo"] == "usageLimitExceeded";
    let pause = state
        .automatic_switching
        .get(thread)
        .and_then(|p| p.last_job.as_ref())
        .filter(|id| Some(*id) == latest_job.as_ref())
        .and_then(|id| jobs.iter().find(|j| j.id == *id))
        .filter(|j| {
            j.kind == "pause"
                && j.phase == "complete"
                && j.source.account == source.account
                && j.steered_turns
                    .get(&format!("{thread}:{turn_id}"))
                    .is_some_and(|s| s == "accepted")
        });
    if !quota && pause.is_none() {
        return Ok(());
    }
    let event = if quota {
        format!("quota:{}:{turn_id}", source.account)
    } else {
        format!("pause:{}", pause.unwrap().id)
    };
    if policy.last_event.as_deref() == Some(&event) {
        return Ok(());
    }
    // Require the account reading to corroborate the stop. A healthy account
    // plus an old failed turn must never manufacture a future auto-start.
    let threshold_pause = !quota;
    let stopped_usage = if threshold_pause {
        state
            .automatic_switching
            .get(thread)
            .and_then(|p| p.exhaustion_usage.as_ref())
            .unwrap_or(&account.usage)
    } else {
        &account.usage
    };
    let windows: Vec<_> = [&stopped_usage.short, &stopped_usage.weekly]
        .into_iter()
        .flatten()
        .filter(|w| {
            w.used_percent
                >= if threshold_pause {
                    if w.duration_minutes == 300 {
                        account.switch_thresholds.short_percent
                    } else {
                        account.switch_thresholds.weekly_percent
                    }
                } else {
                    100.
                }
        })
        .cloned()
        .collect();
    if windows.is_empty() {
        return Ok(());
    }
    let waiting = Waiting {
        event,
        account: source.account.clone(),
        turn: turn_id.into(),
        pane: source.pane.clone(),
        pid: source.pid,
        start_time: source.start_time,
        observed_at: stopped_usage.observed_at,
        windows,
        threshold_pause,
        operations,
    };
    daemon::call(
        paths,
        "auto-start/observe",
        json!({"thread":thread,"revision":policy.revision,"waiting":waiting}),
    )
    .await?;
    Ok(())
}
