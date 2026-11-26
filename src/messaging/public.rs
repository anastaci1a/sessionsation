//! Stable public messaging using the shared durable ledger. Submit/Queue remain
//! non-steering; Resume requires explicit recovery intent. Only the separately
//! requested Steer mode may address an exact active turn, with acceptance-only evidence.
use super::*;
use anyhow::Context;
mod steering;
pub use steering::{Intent as SteeringIntent, target as steering_target};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicMode {
    Submit,
    Queue,
    Resume,
    Steer,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicRequest {
    pub api_version: u32,
    pub message_id: String,
    pub session_id: String,
    pub expected_generation: String,
    pub mode: PublicMode,
    pub text: String,
    pub expires_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<super::recovery::Intent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering: Option<SteeringIntent>,
    /// Caller origin (`operator` or `agent`); auto-detected when omitted. Recorded on the receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Accepted for parity with Claude targets; Codex supports only `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    /// Accepted and ignored for Codex targets (no idle subscription).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub await_reply: bool,
    /// Opt-in fallback for `submit` (contract 1.46.5): `"queue"` queues the input when a turn
    /// started before delivery instead of refusing with `target_busy`. Valid only with `submit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_busy: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Binding {
    #[serde(default)]
    cancelled: bool,
    /// Delivered by the release of a finished compaction (contract 1.47.0): always with queue
    /// semantics, so several held messages line up in the native queue in arrival order.
    #[serde(default)]
    released_hold: bool,
    /// Resolved caller origin and its source (`explicit`, `detected`, `default`);
    /// absent on bindings written before contract 1.42.0.
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    origin_source: Option<String>,
    #[serde(default)]
    evidence_observed_at: Option<i64>,
    /// The unified send mapping (contract 1.64.0); absent on older bindings and internal sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan: Option<crate::provider::send::Plan>,
    request: PublicRequest,
    native: Enqueue,
}

impl Binding {
    /// The original public request, for an idempotent re-send of a held message.
    pub(super) fn into_request(self) -> PublicRequest {
        self.request
    }
    /// A new binding of `request` to its `native` target, its caller origin resolved now.
    fn new(
        paths: &Paths,
        native: Enqueue,
        request: PublicRequest,
        plan: Option<crate::provider::send::Plan>,
    ) -> Self {
        let (origin, source) = crate::caller_origin::resolve(paths, request.origin.as_deref());
        Binding {
            request,
            native,
            cancelled: false,
            released_hold: false,
            origin: Some(origin),
            origin_source: Some(source.to_owned()),
            evidence_observed_at: None,
            plan,
        }
    }
    /// Write the binding row: a new one, or the row of `id` replaced.
    fn put(&self, ledger: &Ledger, id: Option<&str>) -> anyhow::Result<()> {
        let json = serde_json::to_string(self)?;
        if let Some(id) = id {
            let sql = "UPDATE public_messages SET binding=? WHERE id=?";
            ledger.db.execute(sql, params![json, id])?;
        } else {
            let sql = "INSERT INTO public_messages VALUES(?,?)";
            ledger
                .db
                .execute(sql, params![self.request.message_id, json])?;
        }
        Ok(())
    }
}

pub(super) fn get(ledger: &impl Reads, id: &str) -> Result<Option<Binding>> {
    uuid(id)?;
    ledger
        .binding(id)?
        .map(|s| serde_json::from_str(&s).map_err(|_| Fault("storage_error")))
        .transpose()
}

fn wrap(binding: &Binding, value: Value) -> Value {
    if binding.request.mode == PublicMode::Steer {
        return steering::unsubmitted(binding, value["state"].as_str().unwrap_or("pending_local"));
    }
    let accepted = !value["native_queue_id"].is_null() || !value["correlation"].is_null();
    let correlation = &value["correlation"];
    let reply = &value["reply"];
    let observation = value["observation"]["state"]
        .as_str()
        .unwrap_or("not_checked");
    let now = crate::daemon::now();
    let evidence_at = binding.evidence_observed_at;
    let freshness = match evidence_at {
        Some(at) if at <= now && now < at + 5 => "fresh",
        Some(_) => "stale",
        None => "unknown",
    };
    let mut out = json!({"origin":binding.origin.as_deref().unwrap_or("operator"),"origin_source":binding.origin_source.as_deref().unwrap_or("default"),
        "message_id":binding.request.message_id,"session_id":binding.request.session_id,
        "accepted_generation":binding.request.expected_generation,"provider":"codex",
        "accepted_runtime_generation":binding.native.expected_generation,
        "provider_conversation_id":binding.native.session_id,"mode":binding.request.mode,"if_busy":binding.request.if_busy,"hold_release":if binding.released_hold {json!({"from":"compaction","delivered_as":"queue","note":"held while a compaction ran; delivered with queue semantics (native queue, arrival order) after it"})} else {Value::Null},
        "delivery_semantics":"non_steering_native_queue",
        "state":if value["state"]=="unknown" {json!("delivery_unknown")} else {value["state"].clone()},
        "reason":value["reason"],"reason_detail":if value["reason"]=="unsupported_native_version" {Some("The account app-server version is not supported by this Sessionsation build. Use api observe to inspect its native_protocol version and supported_versions; no native input was attempted.")} else if value["reason"]=="compaction_hold" {Some(HOLD_DETAIL)} else if value["reason"]=="queued_after_interrupted_turn" {Some("The last turn was interrupted and Codex never starts queued input after an interrupt. Resume the session (mode resume) or cancel the message (api cancel, or api stop --discard-queued).")} else if value["reason"]=="model_change_hold" {Some("A model change is pending for this session (contract 1.49.0). The message is held locally (nothing is in the native queue) and is delivered on the new model once the change completes, fails, expires or is cancelled.")} else {None},"facts":{"accepted_local":true,"accepted_native":accepted,
            "input_recorded":!value["correlation"].is_null(),"input_consumed":if !value["correlation"].is_null() {json!(true)} else if value["state"]=="failed" && value["reason"]=="input_not_consumed" {json!(false)} else {json!("unknown")},
            "reply_observed":!reply.is_null()},
        "native_queue_id":value["native_queue_id"],"recovery_start":value["recovery_start"],
        "correlation":if correlation.is_null() {Value::Null} else {json!({
            "native_turn_id":correlation["turn_id"],"original_input_item_id":correlation["user_item_id"],
            "completion_input_item_id":correlation["completed_user_item_id"],
            "observed_runtime_generation":correlation["observed_generation"],
            "reply_item_id":reply["item_id"]})},
        "observation":{"state":observation,"checked_at":now,
            "native_attempted_at":if matches!(observation,"observed"|"uncorrelated"|"unavailable") {Some(now)} else {None},
            "evidence_observed_at":evidence_at,"freshness":freshness,"error_code":value["observation"]["code"]},
        "reply":if reply.is_null() {Value::Null} else {json!({"item_id":reply["item_id"],"text":reply["text"],"phase":reply["phase"]})}});
    // Contract 1.58.0: the native transport error of a failed submit, and whether the input
    // provably never reached the app-server (state `rejected`: safe to send again).
    let timing = serde_json::from_value::<Timing>(value["timing"].clone()).ok();
    Timing::facts(timing.as_ref(), &mut out["facts"]);
    if !value["native_error"].is_null() {
        out["facts"]["native_error"] = value["native_error"].clone();
        out["facts"]["retryable"] = json!(value["state"] == "rejected");
    }
    uniform(binding, &mut out);
    out
}

/// Contract 1.64.0: the uniform `delivery` block and `ignored_options` of a Codex receipt.
/// A submit with `if_busy:"queue"` (and a released hold) goes through the native queue.
pub(super) fn uniform(binding: &Binding, out: &mut Value) {
    let r = &binding.request;
    let steer = r.mode == PublicMode::Steer;
    let via_queue = binding.released_hold
        || r.mode == PublicMode::Queue
        || (r.mode == PublicMode::Submit && r.if_busy.as_deref() == Some("queue"));
    let mode_used = if steer {
        "steer"
    } else if via_queue {
        "queue"
    } else {
        "submit"
    };
    let state = out["state"].as_str().unwrap_or_default();
    let queued = !steer && state == "native_accepted";
    let held = state == "pending_local"
        && matches!(
            out["reason"].as_str(),
            Some("compaction_hold" | "model_change_hold")
        );
    let delivery = crate::provider::send::block(
        binding.plan.as_ref(),
        mode_used,
        if steer {
            "native_steer"
        } else {
            "native_queue"
        },
        queued,
        held,
    );
    crate::provider::send::annotate(out, binding.plan.as_ref(), delivery);
}

/// `origin` must be `operator` or `agent`; a `delivery` other than `auto` names a Claude
/// path a Codex target does not have. `await_reply` is ignored for Codex.
fn check_codex_options(request: &PublicRequest) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(request.origin.as_deref(), None | Some("operator" | "agent")),
        "INVALID_REQUEST: origin must be operator or agent"
    );
    match request.delivery.as_deref() {
        None | Some("auto") => Ok::<(), anyhow::Error>(()),
        Some(d @ ("native" | "terminal_input" | "peer_message" | "hosted_stream_json")) => {
            anyhow::bail!(
                "PROVIDER_DELIVERY_UNSUPPORTED: delivery {d} is a Claude path; Codex targets accept only auto"
            )
        }
        Some(_) => anyhow::bail!("INVALID_REQUEST: unknown delivery value"),
    }?;
    match request.if_busy.as_deref() {
        None => Ok(()),
        Some("queue") if request.mode == PublicMode::Submit => Ok(()),
        Some("queue") => {
            anyhow::bail!("INVALID_REQUEST: if_busy is only valid with mode submit")
        }
        Some(_) => anyhow::bail!("INVALID_REQUEST: if_busy must be queue"),
    }
}

pub(crate) const HOLD_DETAIL: &str = "A compaction is queued for this session. The message is held locally (nothing is in the native queue) and is delivered once the compaction completes, fails, expires or is cancelled; cancel it or let it expire to drop it.";
/// Deliver a message a compaction held: marks its binding so it is sent with queue semantics
/// (it may not be the first to start a turn), then runs the ordinary idempotent send.
pub(super) async fn send_released(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    if super::claude_steer::exists(paths, id) {
        return super::claude_steer::send_released(paths, id).await;
    }
    let ledger = ledger::open_write(paths, wait_budget(paths, Wait::Send)).await?;
    let mut binding = get(&ledger, id)?.ok_or(Fault("receipt_not_found"))?;
    if !binding.released_hold {
        binding.released_hold = true;
        binding.put(&ledger, Some(id))?;
    }
    let request = binding.into_request();
    drop(ledger);
    send_public(paths, request).await
}

/// Contract 1.69.3 (`api stop --discard-queued`): cancel the conversation's own still-queued
/// public input through `api cancel`. Never touches a message that may already have run.
pub async fn discard_queued(paths: &Paths, conversation: &str) -> anyhow::Result<Value> {
    let reader = ledger::open_read(paths, wait_budget(paths, Wait::Read)).await?;
    let mut ids = reader.held_ids(conversation, true)?;
    drop(reader);
    ids.extend(super::claude_steer::held_ids(paths, conversation, true));
    let (mut discarded, mut kept) = (vec![], vec![]);
    for id in ids {
        match receipt_public(paths, &id, true).await {
            Ok(v) if v["state"] == "cancelled" => discarded.push(id),
            r => kept.push(json!({"message_id":id,"error":r.err().map(|e| format!("{e:#}"))})),
        }
    }
    // Codex native items no receipt names are deleted by the stop itself (`api stop`).
    Ok(json!({"discarded_messages":discarded,"not_discarded":kept,"discarded_native_items":[]}))
}
/// The Codex native queue items `stop --discard-queued` may delete directly: those no receipt
/// names. An item whose receipt exists (a send that raced the discard) is cancelled through
/// `api cancel` instead and recorded in `d`; one whose cancel failed is kept and reported.
pub async fn discard_items(paths: &Paths, items: &[Value], d: &mut Value) -> Vec<Value> {
    let mut delete = vec![];
    for item in items {
        let id = item["clientUserMessageId"].as_str().unwrap_or_default();
        let kept = |d: &Value| {
            d["not_discarded"]
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k["message_id"] == id))
        };
        if kept(d) {
            continue;
        }
        if !receipt_known(paths, id).await {
            delete.push(item.clone());
            continue;
        }
        let (list, entry) = match receipt_public(paths, id, true).await {
            Ok(v) if v["state"] == "cancelled" => ("discarded_messages", json!(id)),
            r => (
                "not_discarded",
                json!({"message_id":id,"error":r.err().map(|e| format!("{e:#}"))}),
            ),
        };
        if let Some(a) = d[list].as_array_mut() {
            a.push(entry);
        }
    }
    delete
}
/// Whether any receipt store knows this message id (unreadable ledger: assumed known).
async fn receipt_known(paths: &Paths, id: &str) -> bool {
    super::claude_steer::exists(paths, id)
        || super::headless::exists(paths, id)
        || super::recovery::exists(paths, id)
        || match ledger::open_read(paths, wait_budget(paths, Wait::Read)).await {
            Ok(r) => !matches!(r.binding(id), Ok(None)),
            Err(_) => true,
        }
}

pub async fn send_public(paths: &Paths, request: PublicRequest) -> anyhow::Result<Value> {
    send_planned(paths, request, None).await
}

/// [`send_public`] with the unified send mapping of `api send` (contract 1.64.0). An
/// `interrupt` intent with no turn target steers the active turn the session reports, or,
/// with no turn it can steer, is delivered like `deliver` (submit, queued natively when a
/// turn started first), with the reason in `delivery.fallback_reason`.
pub(super) async fn send_planned(
    paths: &Paths,
    mut request: PublicRequest,
    mut plan: Option<crate::provider::send::Plan>,
) -> anyhow::Result<Value> {
    if let Some(p) = plan.as_mut()
        && p.intent() == Some("interrupt")
        && request.mode == PublicMode::Steer
        && request.steering.is_none()
        && request.api_version == crate::api::VERSION
    {
        resolve_interrupt(paths, &mut request, p).await?;
    }
    anyhow::ensure!(
        request.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    check_codex_options(&request)?;
    if request.mode == PublicMode::Steer {
        request
            .steering
            .as_ref()
            .context("STEERING_INTENT_REQUIRED")?
            .validate()?;
    } else {
        anyhow::ensure!(request.steering.is_none(), "STEERING_ONLY_VALID_WITH_STEER");
    }
    if request.mode == PublicMode::Resume {
        return super::recovery::send(paths, request).await;
    }
    anyhow::ensure!(
        request.recovery.is_none(),
        "RECOVERY_ONLY_VALID_WITH_RESUME"
    );
    check_codex_options(&request)?;
    uuid(&request.message_id)?;
    anyhow::ensure!(
        !super::recovery::exists(paths, &request.message_id),
        "MESSAGE_ID_CONFLICT"
    );
    uuid(&request.session_id)?;
    // Real input readiness first (bounded, no ledger lock held): a session launched
    // moments ago can acknowledge input it is not yet reading.
    let mut readiness = std::time::Duration::ZERO;
    if !delivery::timing(paths).ready.is_zero() && request.mode != PublicMode::Steer {
        // A busy ledger refuses before any catalog or native work.
        drop(ledger::open_write(paths, wait_budget(paths, Wait::Send)).await?);
        if let Ok(session) =
            crate::catalog::resolve(paths, &request.session_id, &request.expected_generation)
        {
            if crate::provider::ops::supervised(&session.provider) {
                let started = std::time::Instant::now();
                delivery::await_ready(paths, &session.conversation_id).await;
                readiness = started.elapsed();
            }
        }
    }
    let asked = std::time::Instant::now();
    let mut ledger = ledger::open_write(paths, wait_budget(paths, Wait::Send)).await?;
    let acquired = std::time::Instant::now();
    let binding =
        if let Some(b) = get(&ledger, &request.message_id)? {
            anyhow::ensure!(b.request == request, "MESSAGE_ID_CONFLICT");
            b
        } else {
            anyhow::ensure!(
                ledger.get(&request.message_id)?.is_none(),
                "MESSAGE_ID_CONFLICT"
            );
            super::retention::ensure_not_retired(&ledger, &request.message_id)?;
            let s =
                crate::catalog::resolve(paths, &request.session_id, &request.expected_generation)?;
            crate::provider::ops::require_bare(crate::provider::ops::supervised(&s.provider))?;
            let a = attachment(paths, &s.conversation_id)?;
            anyhow::ensure!(s.attachment.as_ref().is_some_and(|runtime|
            runtime.pid == a.pid && runtime.process_start == a.start), "STALE_TARGET");
            let native = Enqueue {
                message_id: request.message_id.clone(),
                session_id: s.conversation_id,
                text: request.text.clone(),
                expires_at: request.expires_at,
                expected_generation: a.generation(),
            };
            anyhow::ensure!(
                !request.text.is_empty() && request.text.len() <= 16384,
                "INVALID_REQUEST"
            );
            let now = crate::daemon::now();
            anyhow::ensure!(
                request.expires_at > now && request.expires_at <= now + 86400,
                "INVALID_EXPIRY"
            );
            let binding = Binding::new(paths, native, request, plan.clone());
            // Bind the logical request before any native mutation. A crash here is
            // provably unsent; later retries retain the exact original native target.
            let count = |l: &Ledger| -> rusqlite::Result<i64> {
                l.db.query_row("SELECT count(*) FROM public_messages", [], |r| r.get(0))
            };
            let mut count_now = count(&ledger)?;
            if count_now >= super::retention::NEAR_CAPACITY {
                super::retention::maybe_run(&mut ledger, now);
                count_now = count(&ledger)?;
            }
            let count = count_now;
            anyhow::ensure!(count < 10000, "RECEIPT_CAPACITY_REACHED");
            binding.put(&ledger, None)?;
            binding
        };
    if binding.cancelled {
        return Ok(wrap(&binding, json!({"state":"cancelled"})));
    }
    // Contract 1.71.2: recorded with the receipt this admission creates (never on a resend).
    let timing = Timing {
        readiness_ms: Timing::ms(readiness),
        ledger_wait_ms: Timing::ms(acquired - asked),
        ledger_hold_ms: Timing::ms(acquired.elapsed()),
        provider_ms: None,
    };
    let mut receipt = if binding.request.mode == PublicMode::Steer {
        steering::accept(&mut ledger, &binding, timing)?
    } else {
        ledger.accept_timed(&binding.native, crate::daemon::now(), None, Some(timing))?
    };
    ledger.expire(&mut receipt, crate::daemon::now())?;
    if binding.request.mode == PublicMode::Steer {
        if let Err(error) = steering::deliver(paths, &binding, &mut ledger, &mut receipt).await {
            let reason = error.to_string();
            // A concurrent sender of this id may already have settled it: its record wins.
            ledger.amend(&mut receipt, |r| {
                if r.state == "pending_local" {
                    r.state = "rejected".into();
                }
                r.reason = Some(reason);
            })?;
        }
        return Ok(steering::receipt(&binding, &receipt));
    }
    let mut accepted_from_idle = false;
    if matches!(receipt.state.as_str(), "pending_local" | "unknown") {
        match deliver_with_policy(
            paths,
            &mut ledger,
            &mut receipt,
            Some(&binding.native),
            // `if_busy: queue` lets a submit that lost the race with a starting turn
            // queue natively (an idle target still dispatches it at once).
            Some(
                if binding.released_hold
                    || (binding.request.mode == PublicMode::Submit
                        && binding.request.if_busy.as_deref() == Some("queue"))
                {
                    PublicMode::Queue
                } else {
                    binding.request.mode.clone()
                },
            ),
            binding.released_hold,
        )
        .await
        {
            Ok(from_idle) => accepted_from_idle = from_idle,
            Err(error) => {
                let now = crate::daemon::now();
                let transport = super::input_health::native_error(error, now);
                // Before `submit` ran nothing was written to the app-server. A connection
                // that could not be made is a definite failure (`rejected`, safe to send
                // again); a held release keeps retrying as `pending_local`.
                let unsent = receipt.state == "pending_local" && transport.is_some();
                let never_reached =
                    unsent && error.0 == "endpoint_unavailable" && !binding.released_hold;
                ledger.amend(&mut receipt, |r| {
                    if let Some(e) = transport.clone()
                        && (r.state != "unknown" || r.native_error.is_none())
                    {
                        r.native_error = Some(e);
                    }
                    if never_reached && r.state == "pending_local" {
                        r.state = "rejected".into();
                    }
                    if r.state == "pending_local"
                        && matches!(
                            error.0,
                            "target_busy"
                                | "generation_mismatch"
                                | "explicit_resume_required"
                                | "approval_or_user_input_required"
                        )
                    {
                        r.state = "rejected".into();
                    }
                    r.reason = Some(error.0.into());
                })?;
                if unsent {
                    super::input_health::record(
                        paths,
                        &receipt.session_id,
                        "not_delivered",
                        transport,
                        now,
                    );
                }
            }
        }
    }
    let checks = delivery::timing(paths);
    if accepted_from_idle && !checks.verify.is_zero() {
        // Acceptance is not consumption: prove the input left the native queue as a
        // recorded turn input, or report it failed/uncertain. No ledger lock is held
        // while waiting.
        let original = receipt.clone();
        drop(ledger);
        let verdict = delivery::verify(paths, &original, &checks).await;
        delivery::settle(paths, &mut receipt, verdict).await?;
    }
    Ok(wrap(&binding, json!(receipt)))
}

/// Choose the native mode of a Codex `interrupt` (contract 1.64.0). A re-send of a known
/// message id keeps the mode and target its first send bound, so it stays one request.
/// Otherwise the active turn is read (no input is sent): a steerable turn is steered,
/// anything else is delivered like `deliver`, and the submit path's own checks refuse
/// what cannot be sent at all.
async fn resolve_interrupt(
    paths: &Paths,
    request: &mut PublicRequest,
    plan: &mut crate::provider::send::Plan,
) -> anyhow::Result<()> {
    let bound = {
        let ledger = ledger::open_write(paths, wait_budget(paths, Wait::Send)).await?;
        uuid(&request.message_id)?;
        get(&ledger, &request.message_id)?
    };
    if let Some(b) = bound {
        request.mode = b.request.mode.clone();
        request.steering = b.request.steering.clone();
        request.if_busy = b.request.if_busy.clone();
        return Ok(());
    }
    // A target that cannot be read is not steerable; the submit path reports why it cannot
    // be sent either (an unknown session, a stale generation, no live process).
    let target = steering::target(paths, &request.session_id, &request.expected_generation)
        .await
        .unwrap_or_else(|e| json!({"available":false,"reason":e.to_string()}));
    match (
        target["active_turn_id"].as_str(),
        target["runtime_generation"].as_str(),
    ) {
        (Some(turn), Some(runtime)) if target["available"] == true => {
            request.steering = Some(SteeringIntent {
                version: 1,
                expected_turn_id: turn.to_owned(),
                expected_runtime_generation: runtime.to_owned(),
            });
        }
        _ => {
            request.mode = PublicMode::Submit;
            request.if_busy = Some("queue".into());
            plan.fallback_reason = Some(
                target["reason"]
                    .as_str()
                    .unwrap_or("no_active_turn")
                    .to_owned(),
            );
        }
    }
    Ok(())
}

fn reconcile_recovered_stop(
    paths: &Paths,
    binding: &Binding,
    receipt: &Receipt,
) -> anyhow::Result<()> {
    // Native auto-dispatch may win the exact-item start race, or its ack may
    // be lost. Only durable correlation of the originally authorized input
    // can substitute for that acknowledgement; never merely queue acceptance.
    if !binding.cancelled
        && binding.request.mode == PublicMode::Resume
        && receipt.correlation.is_some()
        && matches!(
            receipt.state.as_str(),
            "execution_started" | "completed_with_reply" | "failed"
        )
        && let Some(stop) = binding
            .request
            .recovery
            .as_ref()
            .and_then(|i| i.expected_stop_id.as_deref())
    {
        external::reconcile_recovery_stop_intent(paths, &receipt.session_id, stop)?;
    }
    Ok(())
}

/// `receipt_public` from a shared reader when the answer needs no ledger write, no
/// provider read and no recovery reconciliation: an unknown, retired, cancelled or
/// undispatched id, or a settled queue or submit receipt. `None` sends the caller to
/// the writer path, which re-reads everything.
fn settled_receipt(
    paths: &Paths,
    reader: &LedgerRead,
    id: &str,
    cancelling: bool,
) -> anyhow::Result<Option<Value>> {
    let Some(binding) = get(reader, id)? else {
        if super::recovery::exists(paths, id) {
            anyhow::ensure!(
                !cancelling,
                "RECOVERY_CANCELLATION_REQUIRES_VERIFIED_WORKER_STOP"
            );
            return super::recovery::pending_receipt(paths, id).map(Some);
        }
        super::retention::ensure_not_retired(reader, id)?;
        return Err(Fault("receipt_not_found").into());
    };
    if binding.cancelled {
        return super::recovery::decorate(paths, id, wrap(&binding, json!({"state":"cancelled"})))
            .map(Some);
    }
    let Some(receipt) = reader.get(id)? else {
        if cancelling {
            return Ok(None);
        }
        return super::recovery::decorate(
            paths,
            id,
            wrap(
                &binding,
                json!({"state":"pending_local","reason":"not_dispatched"}),
            ),
        )
        .map(Some);
    };
    // Steering, resume (stop reconciliation), cancellation and `pending_local`
    // (which reading may settle) take the writer path.
    if cancelling
        || matches!(binding.request.mode, PublicMode::Steer | PublicMode::Resume)
        || receipt.state == "pending_local"
    {
        return Ok(None);
    }
    let Some(value) = observe::settled(reader, &receipt, crate::daemon::now())? else {
        return Ok(None);
    };
    super::recovery::decorate(paths, id, wrap(&binding, value)).map(Some)
}

pub async fn receipt_public(paths: &Paths, id: &str, cancelling: bool) -> anyhow::Result<Value> {
    if super::claude_steer::exists(paths, id) {
        return super::claude_steer::receipt(paths, id, cancelling).await;
    }
    if super::headless::exists(paths, id) {
        return super::headless::receipt(paths, id, cancelling);
    }
    let deadline = tokio::time::Instant::now() + wait_budget(paths, Wait::Read);
    {
        let reader = ledger::open_read(paths, remaining(deadline)).await?;
        if let Some(value) = settled_receipt(paths, &reader, id, cancelling)? {
            return Ok(value);
        }
    }
    receipt_write(paths, id, cancelling, remaining(deadline)).await
}

/// The writer path of `receipt_public`: everything under the exclusive lease, which may
/// settle, cancel or observe (releasing the lease around provider I/O).
async fn receipt_write(
    paths: &Paths,
    id: &str,
    cancelling: bool,
    budget: std::time::Duration,
) -> anyhow::Result<Value> {
    let mut ledger = ledger::open_write(paths, budget).await?;
    let Some(mut binding) = get(&ledger, id)? else {
        if super::recovery::exists(paths, id) {
            anyhow::ensure!(
                !cancelling,
                "RECOVERY_CANCELLATION_REQUIRES_VERIFIED_WORKER_STOP"
            );
            return super::recovery::pending_receipt(paths, id);
        }
        super::retention::ensure_not_retired(&ledger, id)?;
        return Err(Fault("receipt_not_found").into());
    };
    if binding.cancelled {
        return super::recovery::decorate(paths, id, wrap(&binding, json!({"state":"cancelled"})));
    }
    let Some(mut receipt) = ledger.get(id)? else {
        if cancelling {
            binding.cancelled = true;
            binding.put(&ledger, Some(id))?;
            return super::recovery::decorate(
                paths,
                id,
                wrap(&binding, json!({"state":"cancelled"})),
            );
        }
        return super::recovery::decorate(
            paths,
            id,
            wrap(
                &binding,
                json!({"state":"pending_local","reason":"not_dispatched"}),
            ),
        );
    };
    if binding.request.mode == PublicMode::Steer {
        ledger.expire(&mut receipt, crate::daemon::now())?;
        if cancelling {
            anyhow::ensure!(
                matches!(
                    receipt.state.as_str(),
                    "pending_local" | "rejected" | "expired" | "cancelled"
                ),
                "STEERING_CANCELLATION_UNSUPPORTED"
            );
            if receipt.state == "pending_local" {
                receipt.set_state(&mut ledger, "cancelled", None)?;
            }
        }
        if !steering::observable(&receipt) {
            let observation = if receipt
                .steer
                .as_ref()
                .is_some_and(|s| s.turn_status.is_some())
            {
                json!({"state":"cached"})
            } else {
                json!({"state":"acceptance_only"})
            };
            return Ok(steering::observed(&binding, &receipt, observation));
        }
        // Read-only item observation without the writer lease; never resends.
        let original = receipt.clone();
        drop(ledger);
        let observation = steering::snapshot(paths, &original).await;
        let mut ledger = ledger::open_write(paths, wait_budget(paths, Wait::Read)).await?;
        binding = get(&ledger, id)?.ok_or(Fault("receipt_not_found"))?;
        receipt = ledger.get(id)?.ok_or(Fault("receipt_not_found"))?;
        let status = if receipt != original {
            json!({"state":"unavailable","code":"receipt_changed_during_observation"})
        } else {
            match observation {
                Ok(Some(observed)) => {
                    steering::commit(&mut ledger, &mut receipt, observed)?;
                    binding.evidence_observed_at = Some(crate::daemon::now());
                    binding.put(&ledger, Some(id))?;
                    json!({"state":"observed"})
                }
                Ok(None) => json!({"state":"uncorrelated"}),
                Err(e) => json!({"state":"unavailable","code":e.0}),
            }
        };
        return Ok(steering::observed(&binding, &receipt, status));
    }
    if !cancelling {
        settle_pending(paths, &mut ledger, &mut receipt)?;
    }
    if cancelling {
        cancel(paths, &mut ledger, &mut receipt).await?;
    }
    let value = if matches!(
        receipt.state.as_str(),
        "unknown" | "native_accepted" | "execution_started"
    ) {
        let original = receipt.clone();
        drop(ledger);
        let observation = observe::snapshot(paths, &original).await;
        ledger = ledger::open_write(paths, wait_budget(paths, Wait::Read)).await?;
        binding = get(&ledger, id)?.ok_or(Fault("receipt_not_found"))?;
        receipt = ledger.get(id)?.ok_or(Fault("receipt_not_found"))?;
        let status = if receipt != original || binding.cancelled {
            json!({"state":"unavailable","code":"receipt_changed_during_observation"})
        } else {
            match observation {
                Ok(Some(observed)) => {
                    observe::commit(&mut ledger, &mut receipt, observed)?;
                    json!({"state":"observed"})
                }
                Ok(None) => json!({"state":"uncorrelated"}),
                Err(e) => json!({"state":"unavailable","code":e.0}),
            }
        };
        let mut value = json!(receipt);
        value["observation"] = status;
        value
    } else {
        observe::result(paths, &mut ledger, &mut receipt).await?
    };
    if value["observation"]["state"] == "observed" {
        binding.evidence_observed_at = Some(crate::daemon::now());
        binding.put(&ledger, Some(id))?;
    }
    reconcile_recovered_stop(paths, &binding, &receipt)?;
    super::recovery::decorate(paths, id, wrap(&binding, value))
}

/// A submission that was admitted but never sent must not stay `pending_local` for ever
/// (contract 1.40.1). Reading it settles it: past its expiry it becomes `expired`, and
/// when the session it was admitted for has been replaced (its generation changed, for
/// example by a move or relaunch) it is `rejected` with `generation_mismatch`. Nothing
/// was sent in either case, so both are safe terminal states.
fn settle_pending(paths: &Paths, ledger: &mut Ledger, receipt: &mut Receipt) -> Result<()> {
    if receipt.state != "pending_local" {
        return Ok(());
    }
    ledger.expire(receipt, crate::daemon::now())?;
    if receipt.state == "pending_local"
        && attachment(paths, &receipt.session_id)
            .is_ok_and(|a| a.generation() != receipt.generation)
    {
        receipt.set_state(ledger, "rejected", Some("generation_mismatch"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// Bind the original logical admission to the one positively verified new runtime.
pub(super) async fn deliver_recovered(
    paths: &Paths,
    request: PublicRequest,
    generation: &str,
) -> anyhow::Result<Value> {
    let session = crate::catalog::resolve(paths, &request.session_id, generation)?;
    let a = attachment(paths, &session.conversation_id)?;
    anyhow::ensure!(
        session
            .attachment
            .as_ref()
            .is_some_and(|r| r.pid == a.pid && r.process_start == a.start),
        "STALE_TARGET"
    );
    // Receipt readers share this lock. Wait before binding or dispatching instead
    // of treating a concurrent observer as a failed recovery. This retries only
    // local lock acquisition, never a native write or the launch itself.
    let mut ledger = ledger::open_write(paths, std::time::Duration::from_secs(10)).await?;
    let binding = if let Some(b) = get(&ledger, &request.message_id)? {
        anyhow::ensure!(b.request == request, "MESSAGE_ID_CONFLICT");
        b
    } else {
        anyhow::ensure!(
            ledger.get(&request.message_id)?.is_none(),
            "MESSAGE_ID_CONFLICT"
        );
        let native = Enqueue {
            message_id: request.message_id.clone(),
            session_id: session.conversation_id,
            text: request.text.clone(),
            expires_at: request.expires_at,
            expected_generation: a.generation(),
        };
        let b = Binding::new(paths, native, request, None);
        b.put(&ledger, None)?;
        b
    };
    let mut r = ledger.accept(&binding.native, crate::daemon::now())?;
    ledger.expire(&mut r, crate::daemon::now())?;
    if let Err(e) = super::recovery::deliver(
        paths,
        &mut ledger,
        &mut r,
        &binding.native,
        &binding.request,
    )
    .await
    {
        if r.state == "pending_local" {
            r.state = "rejected".into();
        }
        r.reason = Some(e.to_string());
        ledger.save(&r)?;
    }
    super::recovery::decorate(paths, &binding.request.message_id, wrap(&binding, json!(r)))
}
