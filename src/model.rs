use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SESSION_NAME_MAX: usize = 30;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Window {
    pub used_percent: f64,
    pub duration_minutes: u64,
    pub resets_at: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}
impl Credits {
    pub fn available(&self) -> bool {
        self.has_credits || self.unlimited
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub short: Option<Window>,
    pub weekly: Option<Window>,
    #[serde(default)]
    pub plan_type: Option<String>,
    /// Durations from a complete native snapshot; None preserves legacy uncertainty.
    #[serde(default)]
    pub reported_windows: Option<Vec<u64>>,
    pub observed_at: i64,
    pub exhausted: bool,
    #[serde(default)]
    pub credits: Option<Credits>,
    #[serde(default)]
    pub reached_type: Option<String>,
    #[serde(default)]
    pub spend_control_reached: Option<bool>,
    #[serde(default)]
    pub ordinary_usage_allowed: Option<bool>,
}
impl Usage {
    pub fn window_absent(&self, duration: u64) -> bool {
        self.reported_windows.as_ref().is_some_and(|windows| {
            !windows.is_empty()
                && windows.iter().all(|d| matches!(d, 300 | 10080))
                && !windows.contains(&duration)
        })
    }
    pub fn windows_complete(&self) -> bool {
        let supported = self.reported_windows.as_ref().is_none_or(|windows| {
            !windows.is_empty() && windows.iter().all(|d| matches!(d, 300 | 10080))
        });
        supported
            && [(300, &self.short), (10080, &self.weekly)]
                .iter()
                .all(|(d, w)| w.is_some() != self.window_absent(*d))
    }
    pub fn observed_fresh(&self, now: i64, stale: u64) -> bool {
        self.observed_at > 0 && now >= self.observed_at && now - self.observed_at < stale as i64
    }
    pub fn fresh(&self, now: i64, stale: u64) -> bool {
        self.observed_fresh(now, stale)
            && self.windows_complete()
            && [&self.short, &self.weekly].into_iter().flatten().all(|w| {
                w.used_percent.is_finite()
                    && (0. ..=100.).contains(&w.used_percent)
                    && w.resets_at > now
            })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    #[serde(default)]
    pub deletion_id: Option<String>,
    #[serde(default)]
    pub switch_thresholds: crate::automatic::Thresholds,
    pub name: String,
    pub identity: String,
    pub enabled: bool,
    pub auth_required: bool,
    pub usage: Usage,
    pub consumed_short: f64,
    pub consumed_weekly: f64,
    pub checked_short: f64,
    pub checked_weekly: f64,
    pub last_assigned: i64,
}
impl Account {
    pub fn new(name: String, identity: String) -> Self {
        Self {
            deletion_id: None,
            switch_thresholds: crate::automatic::Thresholds::default(),
            name,
            identity,
            enabled: true,
            auth_required: false,
            usage: Usage::default(),
            consumed_short: 0.,
            consumed_weekly: 0.,
            checked_short: 0.,
            checked_weekly: 0.,
            last_assigned: 0,
        }
    }
    pub fn observe(&mut self, usage: Usage) {
        if usage.observed_at < self.usage.observed_at {
            return;
        }
        fn delta(old: &Option<Window>, new: &Option<Window>) -> f64 {
            match (old, new) {
                (Some(a), Some(b))
                    if a.resets_at == b.resets_at && a.duration_minutes == b.duration_minutes =>
                {
                    (b.used_percent - a.used_percent).max(0.)
                }
                _ => 0.,
            }
        }
        self.consumed_short += delta(&self.usage.short, &usage.short);
        self.consumed_weekly += delta(&self.usage.weekly, &usage.weekly);
        self.switch_thresholds
            .observe_plan(usage.plan_type.as_deref());
        self.usage = usage;
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Phase {
    Idle,
    Running,
    PauseRequested,
    Quiescent,
    Restarting,
    Restoring,
    WaitingCapacity,
    AuthRequired,
    BlockedToolState,
    Paused,
    RecoveryRequired,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub root_thread: Option<String>,
    pub cwd: String,
    pub account: Option<String>,
    pub phase: Phase,
    pub assigned_at: i64,
    pub waiting_since: i64,
    pub manual_pause: bool,
    pub resume_work: bool,
    pub reason: Option<String>,
    pub handoff: Option<Handoff>,
    pub tmux_pane: Option<String>,
    pub codex_args: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Handoff {
    pub id: String,
    pub source: Option<String>,
    pub destination: Option<String>,
    pub reason: String,
    pub started_at: i64,
    pub continuation: Continuation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Continuation {
    NotNeeded,
    Pending,
    Submitting,
    Confirmed,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub automatic_start: BTreeMap<String, crate::autostart::Policy>,
    #[serde(default)]
    pub automatic_switching: BTreeMap<String, crate::automatic::Policy>,
    pub accounts: BTreeMap<String, Account>,
    pub sessions: BTreeMap<String, Session>,
}

/// A positive snapshot, not an inference from lack of notifications.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Quiescence {
    pub complete_tree_observed: bool,
    pub all_turns_ended: bool,
    pub no_running_tools: bool,
    pub no_pending_approvals: bool,
    pub history_flushed: bool,
    pub no_ambiguous_side_effects: bool,
}
impl Quiescence {
    pub fn verified(&self) -> bool {
        self.complete_tree_observed
            && self.all_turns_ended
            && self.no_running_tools
            && self.no_pending_approvals
            && self.history_flushed
            && self.no_ambiguous_side_effects
    }
}
