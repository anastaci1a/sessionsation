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
    /// Required in stored records; pre-provider registries are upgraded by
    /// `store::migrate_legacy_codex_provider`, never inferred from a name.
    pub provider: crate::provider::ProviderId,
    /// Adopted provider configuration home (Claude only); None uses the managed home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_home: Option<std::path::PathBuf>,
    /// Last provider authentication problem code (e.g. WRONG_AUTH_MODE); no secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_detail: Option<String>,
    /// Human display name; the stable name, id and home never change on rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Persistent display order shared by CLI and UI (0-based, unique; None until migrated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
}
/// The registry key of an account. Names are unique per provider only, so the
/// key is the bare name for Codex (the original, unambiguous enrollment kind;
/// every Codex-only subsystem keeps addressing accounts by bare name) and
/// `<provider>:<name>` for every other provider.
pub fn account_key(provider: crate::provider::ProviderId, name: &str) -> String {
    if provider == crate::provider::ProviderId::Codex {
        name.to_owned()
    } else {
        format!("{provider}:{name}")
    }
}
impl State {
    pub fn account(&self, provider: crate::provider::ProviderId, name: &str) -> Option<&Account> {
        self.accounts.get(&account_key(provider, name))
    }
    pub fn account_mut(
        &mut self,
        provider: crate::provider::ProviderId,
        name: &str,
    ) -> Option<&mut Account> {
        self.accounts.get_mut(&account_key(provider, name))
    }
    pub fn insert_account(&mut self, account: Account) {
        self.accounts.insert(account.key(), account);
    }
    /// Every enrolled account with this bare name (at most one per provider).
    pub fn accounts_named(&self, name: &str) -> Vec<&Account> {
        self.accounts.values().filter(|a| a.name == name).collect()
    }
    /// Resolve a possibly bare name to one account. A bare name that exists under
    /// several providers is ambiguous and must be qualified.
    pub fn resolve_account(
        &self,
        name: &str,
        provider: Option<crate::provider::ProviderId>,
    ) -> anyhow::Result<&Account> {
        if let Some(provider) = provider {
            if let Some(a) = self.account(provider, name) {
                return Ok(a);
            }
            if let [other] = self.accounts_named(name).as_slice() {
                anyhow::bail!(
                    "ACCOUNT_PROVIDER_MISMATCH: {name} is enrolled as {}:{name}, not {provider}:{name}",
                    other.provider
                );
            }
            anyhow::bail!("ACCOUNT_NOT_FOUND: no enrolled account named {provider}:{name}");
        }
        let all = self.accounts_named(name);
        match all.as_slice() {
            [one] => Ok(one),
            [] => anyhow::bail!("ACCOUNT_NOT_FOUND: no enrolled account named {name}"),
            many => anyhow::bail!(
                "ACCOUNT_AMBIGUOUS: '{name}' exists as {}; use <provider>:<name>",
                many.iter()
                    .map(|a| format!("{}:{}", a.provider, a.name))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        }
    }
    /// Accounts in persistent order (then name).
    pub fn ordered(&self) -> Vec<&Account> {
        let mut all: Vec<_> = self.accounts.values().collect();
        all.sort_by_key(|a| (a.order, a.key()));
        all
    }
    /// Idempotent: accounts without an order are appended in alphabetical
    /// order after the highest assigned one (existing accounts keep today's
    /// alphabetical order; a new account goes last). Returns the names changed.
    pub fn normalize_order(&mut self) -> Vec<String> {
        let next = self
            .accounts
            .values()
            .filter_map(|a| a.order)
            .max()
            .map_or(0, |m| m + 1);
        let mut changed = vec![];
        for (offset, a) in self
            .accounts
            .values_mut()
            .filter(|a| a.order.is_none())
            .enumerate()
        {
            a.order = Some(next + offset as u32);
            changed.push(a.key());
        }
        changed
    }
    /// Move one account to a 0-based position and renumber densely.
    pub fn reorder(&mut self, name: &str, position: usize) -> bool {
        self.normalize_order();
        let mut names: Vec<String> = self.ordered().iter().map(|a| a.key()).collect();
        let Some(current) = names.iter().position(|n| n == name) else {
            return false;
        };
        let item = names.remove(current);
        names.insert(position.min(names.len()), item);
        for (i, n) in names.iter().enumerate() {
            self.accounts.get_mut(n).unwrap().order = Some(i as u32);
        }
        current != position.min(names.len() - 1)
    }
}
impl Account {
    /// The registry key (see [`account_key`]).
    pub fn key(&self) -> String {
        account_key(self.provider, &self.name)
    }
    /// A Codex account (the original enrollment kind).
    pub fn new(name: String, identity: String) -> Self {
        Self::with_provider(crate::provider::ProviderId::Codex, name, identity)
    }
    pub fn with_provider(
        provider: crate::provider::ProviderId,
        name: String,
        identity: String,
    ) -> Self {
        Self {
            provider,
            config_home: None,
            auth_detail: None,
            display_name: None,
            order: None,
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

#[cfg(test)]
mod order_tests {
    use super::*;
    #[test]
    fn order_migration_is_alphabetical_idempotent_and_appends_new_accounts() {
        let mut state = State::default();
        for n in ["b", "a", "c"] {
            state
                .accounts
                .insert(n.into(), Account::new(n.into(), String::new()));
        }
        assert_eq!(state.normalize_order(), ["a", "b", "c"]);
        assert!(state.normalize_order().is_empty());
        state
            .accounts
            .insert("0new".into(), Account::new("0new".into(), String::new()));
        state.normalize_order();
        let names = |s: &State| {
            s.ordered()
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&state), ["a", "b", "c", "0new"]);
        assert!(state.reorder("0new", 0));
        assert_eq!(names(&state), ["0new", "a", "b", "c"]);
        assert!(!state.reorder("0new", 0));
        assert!(state.reorder("0new", 99));
        assert_eq!(names(&state), ["a", "b", "c", "0new"]);
    }
}

#[cfg(test)]
mod per_provider_name_tests {
    use super::*;
    use crate::provider::ProviderId;

    fn state() -> State {
        let mut s = State::default();
        s.insert_account(Account::new("work".into(), String::new()));
        s.insert_account(Account::with_provider(
            ProviderId::Claude,
            "work".into(),
            String::new(),
        ));
        s.insert_account(Account::with_provider(
            ProviderId::Gemini,
            "solo".into(),
            String::new(),
        ));
        s
    }
    #[test]
    fn the_same_name_coexists_under_two_providers() {
        let s = state();
        assert_eq!(s.accounts.len(), 3);
        assert!(s.accounts.contains_key("work"), "Codex keeps its bare key");
        assert!(s.accounts.contains_key("claude:work"));
        assert_eq!(
            s.account(ProviderId::Codex, "work").unwrap().provider,
            ProviderId::Codex
        );
        assert_eq!(
            s.account(ProviderId::Claude, "work").unwrap().provider,
            ProviderId::Claude
        );
        assert!(s.account(ProviderId::Gemini, "work").is_none());
    }
    #[test]
    fn bare_names_resolve_only_while_unambiguous() {
        let s = state();
        assert_eq!(
            s.resolve_account("solo", None).unwrap().provider,
            ProviderId::Gemini
        );
        let e = s.resolve_account("work", None).unwrap_err().to_string();
        assert!(e.starts_with("ACCOUNT_AMBIGUOUS"), "{e}");
        assert!(e.contains("codex:work") && e.contains("claude:work"), "{e}");
        assert_eq!(
            s.resolve_account("work", Some(ProviderId::Claude))
                .unwrap()
                .provider,
            ProviderId::Claude
        );
        let e = s
            .resolve_account("solo", Some(ProviderId::Codex))
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("ACCOUNT_PROVIDER_MISMATCH"), "{e}");
        let e = s.resolve_account("nobody", None).unwrap_err().to_string();
        assert!(e.starts_with("ACCOUNT_NOT_FOUND"), "{e}");
    }
    #[test]
    fn ordering_and_reordering_use_unique_keys() {
        let mut s = state();
        assert_eq!(s.normalize_order().len(), 3);
        let before: Vec<_> = s.ordered().iter().map(|a| a.key()).collect();
        assert!(s.reorder("claude:work", 2));
        assert_eq!(s.ordered()[2].key(), "claude:work");
        assert_eq!(s.ordered().len(), before.len());
    }
    #[test]
    fn legacy_registries_are_rekeyed_once_per_provider() {
        let mut value = serde_json::json!({"accounts":{
            "work":{"name":"work","provider":"codex"},
            "home":{"name":"home","provider":"claude"},
            "g":{"name":"g","provider":"gemini"}}});
        let mut moved = crate::store::migrate_account_keys(&mut value);
        moved.sort();
        assert_eq!(moved, ["claude:home", "gemini:g"]);
        let keys: Vec<_> = value["accounts"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["claude:home", "gemini:g", "work"]);
        assert!(crate::store::migrate_account_keys(&mut value).is_empty());
    }
}
