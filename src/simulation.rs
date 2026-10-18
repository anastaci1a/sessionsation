//! Executable reference implementation of the durable handoff protocol.
//! It is kept separate from native execution until the native capability gate passes.
use crate::{
    config::Config,
    model::*,
    scheduler::{self, Decision},
    store::Store,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    Usage {
        account: String,
        usage: Usage,
    },
    WorkStarted {
        session: String,
    },
    WorkEnded {
        session: String,
    },
    Quiescence {
        session: String,
        evidence: Quiescence,
    },
    WorkerExited {
        session: String,
    },
    Restored {
        session: String,
    },
    ContinuationAccepted {
        session: String,
    },
    Pause {
        session: String,
    },
    Resume {
        session: String,
    },
    Tick,
    Crash,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Effect {
    SteerPause { session: String, handoff: String },
    StopWorker { session: String },
    StartWorker { session: String, account: String },
    Continue { session: String, message_id: String },
    Reconcile { session: String },
}

/// Caller must persist state before delivering any returned effects.
pub fn step(state: &mut State, config: &Config, now: i64, input: Input) -> Result<Vec<Effect>> {
    let mut effects = Vec::new();
    match input {
        Input::Usage { account, usage } => state
            .accounts
            .get_mut(&account)
            .ok_or_else(|| anyhow::anyhow!("unknown account"))?
            .observe(usage),
        Input::WorkStarted { session } => {
            let s = get_session(state, &session)?;
            ensure!(
                matches!(s.phase, Phase::Idle | Phase::Running),
                "new work is not admitted in this phase"
            );
            s.phase = Phase::Running;
        }
        Input::WorkEnded { session } => {
            let s = get_session(state, &session)?;
            if s.phase == Phase::Running {
                s.phase = Phase::Idle;
            }
        }
        Input::Quiescence { session, evidence } => {
            let s = get_session(state, &session)?;
            if scheduler::mark_quiescent(s, &evidence) {
                effects.push(Effect::StopWorker { session });
            }
        }
        Input::WorkerExited { session } => {
            let s = get_session(state, &session)?;
            ensure!(
                s.phase == Phase::Quiescent,
                "worker exit without quiescence requires recovery"
            );
            s.account = None;
            let destination = s.handoff.as_ref().and_then(|h| h.destination.clone());
            let manual = s.manual_pause;
            let eligible = destination
                .as_ref()
                .and_then(|a| state.accounts.get(a))
                .is_some_and(|a| scheduler::eligible(a, config, now));
            let s = get_session(state, &session)?;
            if manual {
                s.phase = Phase::Paused;
                s.handoff = None;
            } else if eligible {
                let account = destination.unwrap();
                s.account = Some(account.clone());
                s.phase = Phase::Restoring;
                effects.push(Effect::StartWorker { session, account });
            } else {
                s.phase = Phase::WaitingCapacity;
                s.waiting_since = now;
                if let Some(handoff) = &mut s.handoff {
                    handoff.destination = None;
                }
            }
        }
        Input::Restored { session } => {
            let s = get_session(state, &session)?;
            ensure!(s.phase == Phase::Restoring, "unexpected restoration");
            s.assigned_at = now;
            let account = s.account.clone();
            if s.resume_work {
                let h = s
                    .handoff
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("missing continuation journal"))?;
                ensure!(
                    h.continuation == Continuation::Pending,
                    "ambiguous continuation requires reconciliation"
                );
                h.continuation = Continuation::Submitting;
                effects.push(Effect::Continue {
                    session: session.clone(),
                    message_id: h.id.clone(),
                });
            } else {
                s.phase = Phase::Idle;
                s.handoff = None;
            }
            if let Some(a) = account.and_then(|a| state.accounts.get_mut(&a)) {
                a.last_assigned = now;
            }
        }
        Input::ContinuationAccepted { session } => {
            let s = get_session(state, &session)?;
            ensure!(
                s.phase == Phase::Restoring
                    && s.handoff
                        .as_ref()
                        .is_some_and(|h| h.continuation == Continuation::Submitting),
                "unexpected continuation acknowledgement"
            );
            s.phase = Phase::Running;
            s.handoff = None;
        }
        Input::Pause { session } => {
            let s = get_session(state, &session)?;
            s.manual_pause = true;
            if s.phase == Phase::WaitingCapacity {
                s.phase = Phase::Paused;
            }
        }
        Input::Resume { session } => {
            let s = get_session(state, &session)?;
            s.manual_pause = false;
            if s.phase == Phase::Paused {
                s.phase = Phase::WaitingCapacity;
                s.waiting_since = now;
            }
        }
        Input::Crash => {
            for s in state.sessions.values_mut() {
                if matches!(
                    s.phase,
                    Phase::Running
                        | Phase::Idle
                        | Phase::PauseRequested
                        | Phase::BlockedToolState
                        | Phase::Quiescent
                        | Phase::Restarting
                        | Phase::Restoring
                ) {
                    s.phase = Phase::RecoveryRequired;
                    s.reason = Some(
                        "reconcile existing worker and ambiguous submissions before restarting"
                            .into(),
                    );
                    effects.push(Effect::Reconcile {
                        session: s.id.clone(),
                    });
                }
            }
        }
        Input::Tick => {}
    }
    if let Some(decision) = scheduler::decide(state, config, now) {
        match decision {
            Decision::Pause {
                session,
                destination,
                reason,
            } => {
                let s = get_session(state, &session)?;
                scheduler::begin_handoff(s, destination, reason, now);
                let handoff = s.handoff.as_ref().unwrap().id.clone();
                let account = s.account.clone();
                effects.push(Effect::SteerPause { session, handoff });
                if let Some(a) = account.and_then(|a| state.accounts.get_mut(&a)) {
                    a.checked_short = a.consumed_short;
                    a.checked_weekly = a.consumed_weekly;
                }
            }
            Decision::Start { session, account } => {
                let s = get_session(state, &session)?;
                if let Some(handoff) = &mut s.handoff {
                    handoff.destination = Some(account.clone());
                } else {
                    s.handoff = Some(Handoff {
                        id: uuid::Uuid::new_v4().to_string(),
                        source: None,
                        destination: Some(account.clone()),
                        reason: "capacity available".into(),
                        started_at: now,
                        continuation: if s.resume_work {
                            Continuation::Pending
                        } else {
                            Continuation::NotNeeded
                        },
                    });
                }
                s.account = Some(account.clone());
                s.phase = Phase::Restoring;
                effects.push(Effect::StartWorker { session, account });
            }
        }
    }
    Ok(effects)
}
fn get_session<'a>(state: &'a mut State, id: &str) -> Result<&'a mut Session> {
    state
        .sessions
        .get_mut(id)
        .ok_or_else(|| anyhow::anyhow!("unknown session"))
}

/// Transactional wrapper: callers cannot accidentally dispatch before the journal commit.
pub fn persisted_step(
    store: &mut Store,
    config: &Config,
    now: i64,
    input: Input,
) -> Result<(State, Vec<Effect>)> {
    let mut state = store.load()?;
    let effects = step(&mut state, config, now, input)?;
    store.save(&state, now, "simulation_transition", "")?;
    Ok((state, effects))
}
