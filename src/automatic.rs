//! Persistent automatic-switch preferences and a side-effect-free preview.
//! Experimental execution delegates to the existing manual move worker.
#[cfg(test)]
mod tests;
use crate::{config::Config, model::Account};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const WARNING: &str = "Experimental: may pause active work, move it to a selected account and send a continuation. When none is eligible, it pauses work and disables automation until you re-enable it. Clear unsent drafts before enabling. Live subagent coordination is not verified. Background termination and additional risks still require confirmation. Failed or uncertain moves suspend automation for review.";

/// Published plan ratios are estimates, not token or message guarantees.
/// A generic `pro` report deliberately has no inferred multiplier.
fn plan_multiplier(plan: &str) -> Option<f64> {
    match plan {
        "plus" => Some(1.),
        "pro_5x" | "pro-5x" => Some(5.),
        "pro_20x" | "pro-20x" => Some(20.),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub short_percent: f64,
    pub weekly_percent: f64,
    pub observed_plan: Option<String>,
    pub multiplier: Option<f64>,
    pub needs_plan_review: bool,
    pub customized: bool,
    // Keep unclamped reserve amounts so a downgrade followed by an upgrade
    // restores the original intent, even when the downgrade allows no usage.
    short_reserve: f64,
    weekly_reserve: f64,
}
impl Default for Thresholds {
    fn default() -> Self {
        Self {
            short_percent: 90.,
            weekly_percent: 90.,
            observed_plan: None,
            multiplier: None,
            needs_plan_review: true,
            customized: false,
            short_reserve: 10.,
            weekly_reserve: 10.,
        }
    }
}
impl Thresholds {
    pub fn observe_plan(&mut self, plan: Option<&str>) {
        let Some(plan) = plan.filter(|p| !p.is_empty()) else {
            return;
        };
        if self.observed_plan.as_deref() == Some(plan) {
            return;
        }
        self.observed_plan = Some(plan.to_owned());
        self.needs_plan_review = true;
        if let Some(multiplier) = plan_multiplier(plan) {
            self.apply_multiplier(multiplier);
        } else {
            // Do not carry a tier override into a different detected plan.
            // Preserve both the last percentages and their known reserve basis.
            self.needs_plan_review = true;
        }
    }
    fn apply_multiplier(&mut self, multiplier: f64) {
        if self.multiplier.is_none() && self.customized {
            self.short_reserve = (100. - self.short_percent) * multiplier;
            self.weekly_reserve = (100. - self.weekly_percent) * multiplier;
        }
        self.multiplier = Some(multiplier);
        self.short_percent = (100. - self.short_reserve / multiplier).clamp(0., 100.);
        self.weekly_percent = (100. - self.weekly_reserve / multiplier).clamp(0., 100.);
        self.needs_plan_review = false;
    }
    pub fn set_multiplier(&mut self, multiplier: f64) -> Result<()> {
        ensure!(
            matches!(multiplier, 1. | 5. | 20.),
            "choose a plan ratio of 1, 5 or 20"
        );
        self.apply_multiplier(multiplier);
        Ok(())
    }
    pub fn set_percentages(&mut self, short: f64, weekly: f64) -> Result<()> {
        ensure!(
            [short, weekly]
                .iter()
                .all(|v| v.is_finite() && (0. ..=100.).contains(v)),
            "thresholds must be used percentages between 0 and 100"
        );
        self.short_percent = short;
        self.weekly_percent = weekly;
        self.customized = true;
        let multiplier = self.multiplier.unwrap_or(1.);
        self.short_reserve = (100. - short) * multiplier;
        self.weekly_reserve = (100. - weekly) * multiplier;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Ordered user selection; never broadened to all enrolled accounts.
    pub accounts: Vec<String>,
    pub enabled: bool,
    pub revision: u64,
    pub pending_job: Option<String>,
    pub last_job: Option<String>,
    pub last_switch_at: i64,
    /// Evidence from before an exhaustion pause, retained across a reset while
    /// the pause worker is still reaching a safe stopping point.
    pub exhaustion_usage: Option<crate::model::Usage>,
    pub error: Option<String>,
}

/// Handle preferences and durable reservations on the daemon's single writer.
pub fn request(
    paths: &crate::paths::Paths,
    state: &mut crate::model::State,
    config: &Config,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value> {
    use serde_json::json;
    let thread = params["thread"].as_str().context("missing thread UUID")?;
    uuid::Uuid::parse_str(thread)?;
    let mut policy = state
        .automatic_switching
        .get(thread)
        .cloned()
        .unwrap_or_default();
    match method {
        "automatic/get" => {}
        "automatic/set" => {
            ensure!(
                policy.pending_job.is_none(),
                "an automatic operation is being queued; retry shortly or disable it"
            );
            ensure!(
                !policy.enabled,
                "disable auto-switch before editing its order"
            );
            policy.accounts = serde_json::from_value(params["accounts"].clone())?;
            policy.validate(&state.accounts)?;
            policy.revision += 1;
        }
        "automatic/enable" => {
            ensure!(
                params["accept_experimental"] == true,
                "explicit experimental consent required: {}",
                WARNING
            );
            policy.validate(&state.accounts)?;
            ensure!(policy.accounts.len() >= 2, "select at least two accounts");
            ensure!(
                policy.pending_job.is_none(),
                "an automatic operation is still being queued"
            );
            if let Some(id) = &policy.last_job {
                ensure!(
                    !crate::external::jobs(paths)?
                        .iter()
                        .any(|j| j.id == *id && operation_running(j)),
                    "the previous automatic operation is still running; see Activity"
                );
            }
            for name in &policy.accounts {
                ensure!(
                    !state.accounts[name].switch_thresholds.needs_plan_review,
                    "confirm the plan ratio for {name} in Accounts first"
                );
            }
            policy.enabled = true;
            policy.error = None;
            policy.last_job = None;
            policy.last_switch_at = 0;
            policy.revision += 1;
        }
        "automatic/disable" => {
            policy.enabled = false;
            policy.revision += 1;
        }
        "automatic/suspend" => {
            ensure!(
                params["revision"].as_u64() == Some(policy.revision),
                "preferences changed"
            );
            policy.enabled = false;
            policy.error = Some(
                params["error"]
                    .as_str()
                    .unwrap_or("Automatic operation needs review")
                    .into(),
            );
            policy.revision += 1;
        }
        "automatic/reserve" => {
            ensure!(
                policy.enabled && params["revision"].as_u64() == Some(policy.revision),
                "auto-switch disabled or preferences changed"
            );
            let jobs = crate::external::jobs(paths)?;
            ensure!(
                !state
                    .automatic_switching
                    .values()
                    .any(|p| p.pending_job.is_some()
                        || p.last_job.as_ref().is_some_and(|id| jobs
                            .iter()
                            .any(|j| j.id == *id && !terminal(&j.phase)))),
                "an automatic operation is already in progress"
            );
            let current = params["current_account"]
                .as_str()
                .context("missing current account")?;
            ensure!(
                policy.accounts.iter().any(|a| a == current),
                "current account is outside the rotation"
            );
            let target = params["target"].as_str().context("missing target")?;
            let expected = match preview(
                &policy,
                current,
                &state.accounts,
                config,
                crate::daemon::now(),
            ) {
                Preview::Switch { account } => {
                    ensure!(
                        crate::daemon::now().saturating_sub(policy.last_switch_at)
                            >= config.min_assignment_seconds as i64,
                        "minimum assignment interval has not elapsed"
                    );
                    account
                }
                Preview::Pause => current.to_owned(),
                _ => anyhow::bail!("usage changed; no automatic operation needed"),
            };
            ensure!(target == expected, "usage or target changed");
            policy.exhaustion_usage =
                (target == current).then(|| state.accounts[current].usage.clone());
            policy.pending_job = Some(uuid::Uuid::new_v4().to_string());
        }
        "automatic/finish" => {
            let id = params["reservation"]
                .as_str()
                .context("missing reservation")?;
            ensure!(
                policy.pending_job.as_deref() == Some(id),
                "reservation changed"
            );
            policy.pending_job = None;
            policy.last_switch_at = crate::daemon::now();
            if params["job"].as_str() == Some(id) {
                let job = crate::external::jobs(paths)?
                    .into_iter()
                    .find(|j| j.id == id)
                    .context("job journal missing")?;
                policy.last_job = Some(id.into());
                if job.kind == "pause" {
                    policy.enabled = false;
                    policy.error = Some("No selected account is eligible. Pause queued; see Activity. Re-enable manually when ready.".into());
                }
            } else {
                policy.enabled = false;
                policy.error = Some(
                    params["error"]
                        .as_str()
                        .unwrap_or("Unknown queue outcome; inspect Activity")
                        .into(),
                );
            }
        }
        _ => anyhow::bail!("unknown automatic operation"),
    }
    let value = json!({"policy":policy,"active":policy.enabled,"job":policy.pending_job,"warning":WARNING,
        "preview":preview(&policy,params["current_account"].as_str().unwrap_or(""),&state.accounts,config,crate::daemon::now())});
    if method != "automatic/get" {
        state.automatic_switching.insert(thread.into(), policy);
    }
    Ok(value)
}

fn operation_running(job: &crate::external::Job) -> bool {
    !terminal(&job.phase) || crate::external::handoff_worker_alive(&job.id)
}
fn terminal(phase: &str) -> bool {
    matches!(phase, "complete" | "failed" | "cancelled" | "recovered")
}

/// A crash may have happened on either side of spawning the worker. Never replay
/// the reservation; expose its journal, if any, and require renewed consent.
pub fn reconcile(state: &mut crate::model::State) {
    for policy in state.automatic_switching.values_mut() {
        if let Some(id) = policy.pending_job.take() {
            policy.last_job = Some(id);
            policy.enabled = false;
            policy.error = Some("Supervisor restarted while queuing an operation. Inspect Activity before re-enabling.".into());
            policy.revision += 1;
        }
    }
    for account in state.accounts.values_mut() {
        account
            .switch_thresholds
            .observe_plan(account.usage.plan_type.as_deref());
    }
}

/// A single monitor serializes experimental moves across sessions. Reservations
/// are persisted by the daemon before launching an existing move worker.
pub async fn monitor(paths: crate::paths::Paths) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        // An inspection error causes no action and is retried on the next tick.
        let _ = tick(&paths).await;
        let _ = crate::autostart::tick(&paths).await;
    }
}
async fn tick(paths: &crate::paths::Paths) -> Result<()> {
    use crate::{daemon, external, store};
    use serde_json::json;
    let state = store::load_state_snapshot(paths)?;
    if !state
        .automatic_switching
        .values()
        .any(|p| p.enabled || p.pending_job.is_some())
    {
        return Ok(());
    }
    let jobs = external::jobs(paths)?;
    // This single monitor has no queue call running across ticks. A reservation
    // left from a lost reply is resolved from its journal, never executed again.
    for (thread, policy) in &state.automatic_switching {
        if let Some(id) = &policy.pending_job {
            let outcome = if jobs.iter().any(|j| j.id == *id) {
                json!({"thread":thread,"reservation":id,"job":id})
            } else {
                json!({"thread":thread,"reservation":id,"error":"Queue outcome unavailable. No retry was attempted; inspect Activity."})
            };
            daemon::call(paths, "automatic/finish", outcome).await?;
            return Ok(());
        }
    }
    // Include disabled policies: an already queued operation remains durable.
    if state.automatic_switching.values().any(|p| {
        p.last_job
            .as_ref()
            .is_some_and(|id| jobs.iter().any(|j| j.id == *id && operation_running(j)))
    }) {
        return Ok(());
    }
    let config = store::load_config(paths)?;
    let sources = external::live_sessions(paths).await?;
    for (thread, policy) in &state.automatic_switching {
        if !policy.enabled {
            continue;
        }
        if let Some(id) = &policy.last_job {
            let completed = jobs.iter().find(|j| j.id == *id);
            if completed.is_none_or(|j| {
                j.phase != "complete"
                    || (j.resume_after_move
                        && !matches!(j.continuation_delivery.as_str(), "accepted" | "skipped"))
            }) {
                daemon::call(paths,"automatic/suspend",json!({"thread":thread,"revision":policy.revision,"error":"Last automatic move failed, was cancelled, or needs review. Inspect Activity before re-enabling."})).await?;
                continue;
            }
        }
        let live: Vec<_> = sources
            .iter()
            .filter(|s| !s.inactive && !s.tmux_dead && s.thread_id == *thread)
            .collect();
        if live.len() != 1 {
            continue;
        }
        let source = live[0];
        // Never inject a pause request through a potentially edited composer.
        if !source.shared_server || !state.accounts.contains_key(&source.account) {
            continue;
        }
        if state.accounts[&source.account].usage.observed_at <= policy.last_switch_at {
            continue;
        }
        if !policy.accounts.contains(&source.account) {
            continue;
        }
        let account = match preview(
            policy,
            &source.account,
            &state.accounts,
            &config,
            daemon::now(),
        ) {
            Preview::Switch { account } => {
                if daemon::now().saturating_sub(policy.last_switch_at)
                    < config.min_assignment_seconds as i64
                {
                    continue;
                }
                account
            }
            Preview::Pause => source.account.clone(),
            _ => continue,
        };
        let reservation = daemon::call(paths,"automatic/reserve",json!({"thread":thread,"revision":policy.revision,"current_account":source.account,"target":account})).await?;
        let id = reservation["job"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing automatic reservation"))?;
        let result =
            external::queue_automatic(paths, &source.pane, thread, &source.account, &account, id)
                .await;
        let outcome = match result {
            Ok(job) => json!({"thread":thread,"reservation":id,"job":job.id}),
            Err(error) => json!({"thread":thread,"reservation":id,"error":error.to_string()}),
        };
        daemon::call(paths, "automatic/finish", outcome).await?;
        break;
    }
    Ok(())
}
impl Policy {
    pub fn validate(&self, accounts: &BTreeMap<String, Account>) -> Result<()> {
        ensure!(
            self.accounts.is_empty() || self.accounts.len() >= 2,
            "select at least two accounts, or clear the selection"
        );
        let mut seen = BTreeSet::new();
        for name in &self.accounts {
            ensure!(accounts.contains_key(name), "unknown account {name}");
            ensure!(
                accounts[name].provider == crate::provider::ProviderId::Codex,
                "UNSUPPORTED_PROVIDER_OPERATION: automatic switching supports only codex accounts ({name})"
            );
            ensure!(seen.insert(name), "duplicate account {name}");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Preview {
    Off,
    Stay { account: String },
    Switch { account: String },
    Pause,
    Wait { reason: String },
}
fn usable(a: &Account, config: &Config, now: i64) -> bool {
    a.enabled
        && !a.auth_required
        && !a.switch_thresholds.needs_plan_review
        && a.usage.fresh(now, config.stale_seconds)
        && !a.usage.exhausted
        && a.usage.ordinary_usage_allowed != Some(false)
        && a.usage.spend_control_reached != Some(true)
        && a.usage
            .short
            .as_ref()
            .is_none_or(|w| w.used_percent < a.switch_thresholds.short_percent)
        && a.usage
            .weekly
            .as_ref()
            .is_none_or(|w| w.used_percent < a.switch_thresholds.weekly_percent)
}

/// Sticky ordered selection: do not rebalance while the current account fits.
/// This previews policy only; it cannot pause, launch or move a conversation.
pub fn preview(
    policy: &Policy,
    current: &str,
    accounts: &BTreeMap<String, Account>,
    config: &Config,
    now: i64,
) -> Preview {
    if policy.accounts.is_empty() {
        return Preview::Off;
    }
    if let Err(error) = policy.validate(accounts) {
        return Preview::Wait {
            reason: error.to_string(),
        };
    }
    let position = policy.accounts.iter().position(|name| name == current);
    if position.is_some() {
        let a = &accounts[current];
        // Lack of evidence must not trigger a migration of a working session.
        if !a.usage.fresh(now, config.stale_seconds) || a.switch_thresholds.needs_plan_review {
            return Preview::Wait {
                reason: "current account needs fresh usage or plan review".into(),
            };
        }
        if usable(a, config, now) {
            return Preview::Stay {
                account: current.into(),
            };
        }
    }
    let start = position.map_or(0, |p| (p + 1) % policy.accounts.len());
    for offset in 0..policy.accounts.len() {
        let name = &policy.accounts[(start + offset) % policy.accounts.len()];
        if name != current && usable(&accounts[name], config, now) {
            return Preview::Switch {
                account: name.clone(),
            };
        }
    }
    if position.is_some() {
        Preview::Pause
    } else {
        Preview::Wait {
            reason: "current account is outside the selected rotation".into(),
        }
    }
}
