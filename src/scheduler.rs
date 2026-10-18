use crate::{config::Config, model::*};

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Pause {
        session: String,
        destination: Option<String>,
        reason: String,
    },
    Start {
        session: String,
        account: String,
    },
}

pub fn pressure(account: &Account, config: &Config) -> f64 {
    if !account.usage.windows_complete() {
        return f64::INFINITY;
    }
    [
        (&account.usage.short, config.short_ceiling),
        (&account.usage.weekly, config.weekly_ceiling),
    ]
    .into_iter()
    .filter_map(|(w, ceiling)| w.as_ref().map(|w| w.used_percent / ceiling))
    .fold(0., f64::max)
}
pub fn eligible(account: &Account, config: &Config, now: i64) -> bool {
    let usage = &account.usage;
    if !account.enabled
        || account.auth_required
        || !usage.observed_fresh(now, config.stale_seconds)
        || usage.spend_control_reached == Some(true)
    {
        return false;
    }
    // A reached plan allowance can still be served from purchased credits.
    // Other reached types include credit depletion and workspace limits.
    if (usage.exhausted || usage.ordinary_usage_allowed == Some(false))
        && usage.credits.as_ref().is_some_and(|c| c.available())
        && (usage.reached_type.is_none()
            || usage.reached_type.as_deref() == Some("rate_limit_reached"))
    {
        return true;
    }
    !usage.exhausted
        && usage.ordinary_usage_allowed != Some(false)
        && usage.fresh(now, config.stale_seconds)
        && account
            .usage
            .short
            .as_ref()
            .is_none_or(|w| w.used_percent < config.short_ceiling - config.drain_margin)
        && account
            .usage
            .weekly
            .as_ref()
            .is_none_or(|w| w.used_percent < config.weekly_ceiling - config.drain_margin)
}
pub fn choose<'a>(
    state: &'a State,
    config: &Config,
    now: i64,
    exclude: Option<&str>,
) -> Option<&'a Account> {
    let load = |name: &str| {
        state
            .sessions
            .values()
            .filter(|s| {
                s.account.as_deref() == Some(name)
                    || s.handoff.as_ref().and_then(|h| h.destination.as_deref()) == Some(name)
            })
            .count()
    };
    state
        .accounts
        .values()
        .filter(|a| {
            Some(a.name.as_str()) != exclude
                && eligible(a, config, now)
                // A completed assignment invalidates that observation for another
                // admission, but does not spuriously drain already running work.
                && a.usage.observed_at > a.last_assigned
                // Reserve at most one in-flight handoff per destination account.
                && !state.sessions.values().any(|s| {
                    s.handoff.as_ref().and_then(|h| h.destination.as_deref())
                        == Some(a.name.as_str())
                })
        })
        .min_by(|a, b| {
            pressure(a, config)
                .total_cmp(&pressure(b, config))
                .then(load(&a.name).cmp(&load(&b.name)))
                .then(a.last_assigned.cmp(&b.last_assigned))
                .then(a.name.cmp(&b.name))
        })
}

/// One decision per reconciliation. Persist its reservation before calling again.
pub fn decide(state: &State, config: &Config, now: i64) -> Option<Decision> {
    for session in state
        .sessions
        .values()
        .filter(|s| matches!(s.phase, Phase::Running | Phase::Idle))
    {
        let account = session.account.as_ref().and_then(|a| state.accounts.get(a));
        let reason = if session.manual_pause {
            Some("manual pause")
        } else if account.is_none() {
            Some("account unavailable")
        } else if !eligible(account.unwrap(), config, now) {
            Some("usage, authentication, or freshness reserve")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Some(Decision::Pause {
                session: session.id.clone(),
                destination: if session.manual_pause {
                    None
                } else {
                    choose(state, config, now, session.account.as_deref()).map(|a| a.name.clone())
                },
                reason: reason.into(),
            });
        }
    }
    // Fair recovery comes before optional balancing.
    let mut waiting: Vec<_> = state
        .sessions
        .values()
        .filter(|s| {
            !s.manual_pause && matches!(s.phase, Phase::WaitingCapacity | Phase::AuthRequired)
        })
        .collect();
    waiting.sort_by_key(|s| (s.waiting_since, &s.id));
    if let (Some(session), Some(account)) = (waiting.first(), choose(state, config, now, None)) {
        return Some(Decision::Start {
            session: session.id.clone(),
            account: account.name.clone(),
        });
    }
    // Avoid a cascade of balancing migrations before fresh post-handoff readings.
    if state.sessions.values().any(|s| s.handoff.is_some()) {
        return None;
    }
    for session in state.sessions.values().filter(|s| {
        s.phase == Phase::Running
            && !s.manual_pause
            && now.saturating_sub(s.assigned_at) >= config.min_assignment_seconds as i64
    }) {
        let Some(account) = session.account.as_ref().and_then(|a| state.accounts.get(a)) else {
            continue;
        };
        let due = account.consumed_short - account.checked_short >= config.balance_increment
            || account.consumed_weekly - account.checked_weekly >= config.balance_increment;
        if !due {
            continue;
        }
        if let Some(destination) = choose(state, config, now, Some(&account.name)) {
            if pressure(account, config) - pressure(destination, config) + 1e-9
                >= config.balance_hysteresis
            {
                return Some(Decision::Pause {
                    session: session.id.clone(),
                    destination: Some(destination.name.clone()),
                    reason: "balance checkpoint".into(),
                });
            }
        }
    }
    None
}

pub fn begin_handoff(session: &mut Session, destination: Option<String>, reason: String, now: i64) {
    session.resume_work = session.phase == Phase::Running;
    session.phase = Phase::PauseRequested;
    session.reason = Some(reason.clone());
    session.handoff = Some(Handoff {
        id: uuid::Uuid::new_v4().to_string(),
        source: session.account.clone(),
        destination,
        reason,
        started_at: now,
        continuation: if session.resume_work {
            Continuation::Pending
        } else {
            Continuation::NotNeeded
        },
    });
}

/// No elapsed timeout or model acknowledgement can substitute for this check.
pub fn mark_quiescent(session: &mut Session, evidence: &Quiescence) -> bool {
    if !matches!(
        session.phase,
        Phase::PauseRequested | Phase::BlockedToolState
    ) {
        return false;
    }
    if evidence.verified() {
        session.phase = Phase::Quiescent;
        session.reason = None;
        true
    } else {
        session.phase = Phase::BlockedToolState;
        session.reason = Some("waiting for verified tree/tool quiescence".into());
        false
    }
}
