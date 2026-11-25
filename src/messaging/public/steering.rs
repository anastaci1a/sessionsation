//! Deliberate in-turn steering. No queue fallback, launch or automatic replay.
//! Replies are attributed after the steer's own native input item (revision 15).
use super::*;
use std::time::Duration;
mod reply;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub version: u32,
    pub expected_turn_id: String,
    pub expected_runtime_generation: String,
}
impl Intent {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "UNSUPPORTED_STEERING_VERSION");
        anyhow::ensure!(
            !self.expected_turn_id.is_empty()
                && self.expected_turn_id.len() <= 256
                && !self.expected_turn_id.chars().any(char::is_control),
            "INVALID_ACTIVE_TURN"
        );
        anyhow::ensure!(
            self.expected_runtime_generation.len() == 64
                && self
                    .expected_runtime_generation
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "INVALID_RUNTIME_GENERATION"
        );
        Ok(())
    }
}
#[derive(Deserialize)]
struct Read {
    thread: Thread,
}
#[derive(Deserialize)]
struct Thread {
    id: String,
    status: Status,
}
#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Status {
    r#type: String,
    #[serde(default)]
    active_flags: Vec<String>,
}
#[derive(Deserialize)]
struct Page {
    data: Vec<Turn>,
}
#[derive(Deserialize)]
struct Turn {
    id: String,
    status: String,
}
async fn status(c: &mut Transport, id: &str) -> Result<Status> {
    let r: Read = c
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await?;
    if r.thread.id != id {
        return Err(Fault("correlation_changed"));
    }
    if !r.thread.status.active_flags.is_empty() {
        return Err(Fault("approval_or_user_input_required"));
    }
    if r.thread.status.r#type != "active" {
        return Err(Fault("steer_requires_active_turn"));
    }
    Ok(r.thread.status)
}
async fn active_turn(c: &mut Transport, id: &str) -> Result<String> {
    let before = status(c, id).await?;
    let page: Page = c.passive_summary(id).await?;
    let turn = page
        .data
        .first()
        .filter(|t| {
            page.data.len() == 1
                && t.status == "inProgress"
                && !t.id.is_empty()
                && t.id.len() <= 256
                && !t.id.chars().any(char::is_control)
        })
        .ok_or(Fault("active_turn_unverified"))?;
    if status(c, id).await? != before {
        return Err(Fault("state_changed"));
    }
    Ok(turn.id.clone())
}
async fn connect_active(a: &Attachment) -> Result<(Transport, String)> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut c = a.connect_steer().await?;
        let turn = active_turn(&mut c, &a.id).await?;
        Ok((c, turn))
    })
    .await
    .map_err(|_| Fault("observation_timeout"))?
}
fn permitted(paths: &Paths, id: &str) -> Result<()> {
    if !modes::queue::sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    if external::messaging_stop_intent(paths, id)
        .map_err(|_| Fault("discovery_unavailable"))?
        .is_some()
    {
        return Err(Fault("explicit_resume_required"));
    }
    // Contract 1.48.1: no steering while a compaction is queued or running for the thread. A
    // steer into the dedicated compaction turn is refused by the app-server, and one into the
    // turn before it lands ahead of the compaction that then summarizes it away, so a sender
    // must submit instead (held, then delivered as a queued turn after the compaction).
    // Checked under session ownership, which the compaction start also takes.
    if crate::messaging::compaction::hold_active(paths, id) {
        return Err(Fault("target_compacting"));
    }
    Ok(())
}
/// Explicit, bounded read-only lookup. Ordinary polling does not reconstruct busy history.
pub async fn target(paths: &Paths, id: &str, generation: &str) -> anyhow::Result<Value> {
    let s = crate::catalog::resolve(paths, id, generation)?;
    crate::provider::ops::require_bare(crate::provider::ops::supervised(&s.provider))?;
    anyhow::ensure!(
        !matches!(s.account.as_str(), "external" | "unknown")
            && !crate::accounts::removed_session(paths, &s.provider, &s.account)?,
        "ACCOUNT_UNAVAILABLE"
    );
    let a = readiness::recorded(paths, &s)?.ok_or(Fault("no_live_attachment"))?;
    let runtime = a.generation();
    let checked = async {
        let _guard = mutation_lock_waiting(paths, &a.id, wait_budget(paths, Wait::Read)).await?;
        permitted(paths, &a.id)?;
        let (_, turn) = connect_active(&a).await?;
        Ok::<_, Fault>(turn)
    }
    .await;
    crate::catalog::resolve(paths, id, generation)?;
    anyhow::ensure!(
        readiness::recorded(paths, &s)?
            .as_ref()
            .map(Attachment::generation)
            == Some(runtime.clone()),
        "STALE_TARGET"
    );
    let now = crate::daemon::now();
    Ok(
        json!({"version":1,"session_id":id,"generation":generation,"runtime_generation":runtime,"active_turn_id":checked.as_ref().ok(),"available":checked.is_ok(),"reason":checked.as_ref().err().map(|e|e.0),"checked_at":now,"observed_at":if checked.is_ok(){Some(now)}else{None},"valid_until":if checked.is_ok(){Some(now+5)}else{None},"sends_input":false,"starts_process":false,"acceptance_only":false,"reply_correlation":"after_steer_item"}),
    )
}

pub(super) fn accept(db: &mut Ledger, b: &Binding) -> Result<Receipt> {
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&b.request).map_err(|_| Fault("invalid_request"))?)
    );
    db.accept_bound(
        &b.native,
        crate::daemon::now(),
        Some((
            &hash,
            "public_steer",
            Some(SubmitEvidence {
                expected_turn_id: b
                    .request
                    .steering
                    .as_ref()
                    .ok_or(Fault("invalid_request"))?
                    .expected_turn_id
                    .clone(),
                acknowledged_turn_id: None,
                action: Some("public_steer".into()),
                resume: false,
            }),
        )),
    )
}
pub(super) async fn deliver(
    paths: &Paths,
    b: &Binding,
    db: &mut Ledger,
    r: &mut Receipt,
) -> anyhow::Result<()> {
    // Even a lost acknowledgement or a process restart must not repeat steering.
    if r.state != "pending_local" {
        return Ok(());
    }
    let intent = b
        .request
        .steering
        .as_ref()
        .context("STEERING_INTENT_REQUIRED")?;
    // Provider I/O runs without the root-wide lease. Session ownership is awaited
    // (bounded) instead of rejecting with `operation_busy`; lock order is session
    // first, ledger second.
    let budget = wait_budget(paths, Wait::Send);
    let (_guard, a, mut c, turn, anchor) = db
        .during(budget, async {
            let guard = mutation_lock_waiting(paths, &b.native.session_id, budget).await?;
            let session = crate::catalog::resolve(
                paths,
                &b.request.session_id,
                &b.request.expected_generation,
            )?;
            anyhow::ensure!(
                !matches!(session.account.as_str(), "external" | "unknown")
                    && !crate::accounts::removed_session(
                        paths,
                        &session.provider,
                        &session.account
                    )?,
                "ACCOUNT_UNAVAILABLE"
            );
            let a = attachment(paths, &b.native.session_id)?;
            anyhow::ensure!(
                a.generation() == b.native.expected_generation
                    && a.generation() == intent.expected_runtime_generation,
                "RUNTIME_GENERATION_MISMATCH"
            );
            permitted(paths, &a.id)?;
            let (mut c, turn) = connect_active(&a).await?;
            anyhow::ensure!(turn == intent.expected_turn_id, "ACTIVE_TURN_CHANGED");
            // Item positions before the steer; a failed read is still provably unsent.
            let anchor = reply::anchor(&mut c, &a.id, &turn, &b.request.message_id).await?;
            crate::catalog::resolve(paths, &b.request.session_id, &b.request.expected_generation)?;
            anyhow::ensure!(
                attachment(paths, &a.id)?.generation() == intent.expected_runtime_generation,
                "RUNTIME_GENERATION_MISMATCH"
            );
            anyhow::Ok((guard, a, c, turn, anchor))
        })
        .await??;
    // Another process may have settled this id while the lease was released.
    if !db.unchanged(r)? {
        return Ok(());
    }
    db.expire(r, crate::daemon::now())?;
    if r.state != "pending_local" {
        return Ok(());
    }
    r.state = "unknown".into();
    r.reason = Some("steer_dispatch_intent".into());
    r.submit = Some(SubmitEvidence {
        expected_turn_id: turn.clone(),
        acknowledged_turn_id: None,
        action: Some("public_steer".into()),
        resume: false,
    });
    r.steer = Some(SteerEvidence {
        anchor: Some(anchor),
        ..Default::default()
    });
    // Durable uncertainty, saved under the lease, precedes the first possible
    // native write: every other process sees `unknown` and never steers this id.
    db.save(r)?;
    let outcome = db
        .during(
            budget,
            c.steer(&a.id, &b.request.message_id, &turn, &b.request.text),
        )
        .await?;
    // A concurrent observation may already have advanced the receipt: never regress it.
    if !db.unchanged(r)? {
        return Ok(());
    }
    match outcome {
        Ok(ack) => {
            r.state = "native_accepted".into();
            r.reason = None;
            r.submit.as_mut().unwrap().acknowledged_turn_id = Some(ack);
        }
        Err(e) => {
            if matches!(
                e.0,
                "compact_turn_not_steerable" | "review_turn_not_steerable"
            ) {
                // Verified native pre-admission rejection, not an uncertain injection.
                r.state = "rejected".into();
            }
            r.reason = Some(e.0.into());
        } // A generic rejection cannot prove non-consumption.
    }
    db.save(r)?;
    Ok(())
}

pub(super) fn unsubmitted(b: &Binding, state: &str) -> Value {
    let r = Receipt {
        recovery_start: None,
        message_id: b.request.message_id.clone(),
        session_id: b.native.session_id.clone(),
        generation: b.native.expected_generation.clone(),
        state: state.into(),
        native_queue_id: None,
        reason: None,
        correlation: None,
        reply: None,
        submit: None,
        steer: None,
        native_error: None,
    };
    receipt(b, &r)
}
pub(super) fn receipt(b: &Binding, r: &Receipt) -> Value {
    project(b, r, json!({"state":"acceptance_only"}))
}
/// Whether a receipt can still gain native item evidence by observation.
pub(super) fn observable(r: &Receipt) -> bool {
    r.submit.is_some()
        && matches!(
            r.state.as_str(),
            "unknown" | "native_accepted" | "execution_started"
        )
}
pub(super) use reply::snapshot;
/// Terminal durable evidence is never regressed by a later observation.
pub(super) fn commit(db: &mut Ledger, r: &mut Receipt, observed: Receipt) -> Result<()> {
    if reply::terminal(&r.state) {
        return Ok(());
    }
    if *r != observed {
        db.save(&observed)?;
        *r = observed;
    }
    Ok(())
}
pub(super) fn observed(b: &Binding, r: &Receipt, observation: Value) -> Value {
    project(b, r, observation)
}
fn project(b: &Binding, r: &Receipt, observation: Value) -> Value {
    let state = r.state.as_str();
    let ack = r
        .submit
        .as_ref()
        .and_then(|e| e.acknowledged_turn_id.as_ref());
    let steer = r.steer.clone().unwrap_or_default();
    let recorded = steer.input_item_id.is_some();
    let not_consumed = r.reason.as_deref() == Some("input_not_consumed");
    let final_reply = state == "completed_with_reply";
    let now = crate::daemon::now();
    let freshness = match b.evidence_observed_at {
        Some(at) if at <= now && now < at + 5 => "fresh",
        Some(_) => "stale",
        None => "unknown",
    };
    let obs = observation["state"].as_str().unwrap_or("acceptance_only");
    let mut out = json!({"origin":b.origin.as_deref().unwrap_or("operator"),"origin_source":b.origin_source.as_deref().unwrap_or("default"),"message_id":b.request.message_id,"session_id":b.request.session_id,
        "accepted_generation":b.request.expected_generation,"accepted_runtime_generation":b.native.expected_generation,
        "provider":"codex","provider_conversation_id":b.native.session_id,"mode":"steer",
        "delivery_semantics":"deliberate_in_turn_steer","state":if state=="unknown" {"delivery_unknown"} else {state},
        "reason":r.reason,"expected_turn_id":b.request.steering.as_ref().map(|i|&i.expected_turn_id),
        "acknowledged_turn_id":ack,"acceptance_only":false,"reply_correlation":"after_steer_item",
        "facts":{"accepted_local":true,"accepted_native":ack.is_some()||recorded,
            "native_acceptance":if ack.is_some(){"accepted"}else if recorded{"input_observed"}else if state=="unknown"{"unknown"}else{"not_attempted"},
            "input_recorded":recorded,
            "input_consumed":if not_consumed {json!(false)} else if recorded && (steer.agent_items_after>0 || steer.turn_status.as_deref()==Some("completed")) {json!(true)} else {json!("unknown")},
            "turn_completed":match steer.turn_status.as_deref() {Some("completed")=>json!(true),Some(_)=>json!(false),None=>json!("unknown")},
            "turn_status":steer.turn_status,"reply_observed":r.reply.is_some(),"reply_final":final_reply,
            "shared_with_turn_tail":steer.shared_with_turn_tail},
        "native_queue_id":null,
        "correlation":if recorded {json!({"native_turn_id":r.submit.as_ref().map(|s|&s.expected_turn_id),
            "steer_input_item_id":steer.input_item_id,"completion_input_item_id":steer.completed_input_item_id,"next_input_item_id":steer.next_input_item_id,
            "reply_item_id":r.reply.as_ref().map(|x|&x.item_id),"observed_runtime_generation":steer.observed_generation,
            "anchor":steer.anchor.as_ref().map(|a|json!({"user_items_before":a.user_items_before,"items_before":a.items_before,"last_agent_item_id":a.last_agent_item_id}))})} else {Value::Null},
        "reply":r.reply.as_ref().map(|x|json!({"item_id":x.item_id,"text":x.text,"phase":x.phase,"final":final_reply,"shared_with_turn_tail":steer.shared_with_turn_tail})),
        "observation":{"state":obs,"checked_at":now,
            "native_attempted_at":if matches!(obs,"observed"|"uncorrelated"|"unavailable") {Some(now)} else {None},
            "error_code":observation["code"],"freshness":if obs=="acceptance_only" {"unknown"} else {freshness},
            "evidence_observed_at":b.evidence_observed_at}});
    super::uniform(b, &mut out);
    out
}

#[cfg(test)]
mod tests;
