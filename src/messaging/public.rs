//! Stable public messaging using the shared durable ledger. Submit/Queue remain
//! non-steering; Resume requires explicit recovery intent. Only the separately
//! requested Steer mode may address an exact active turn, with acceptance-only evidence.
use super::*;
use anyhow::Context;
use rusqlite::OptionalExtension;
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
    request: PublicRequest,
    native: Enqueue,
}

impl Binding {
    /// The original public request, for an idempotent re-send of a held message.
    pub(super) fn into_request(self) -> PublicRequest {
        self.request
    }
}

pub(super) fn initialize(ledger: &Ledger) -> Result<()> {
    ledger.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS public_messages(id TEXT PRIMARY KEY, binding TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(super) fn get(ledger: &Ledger, id: &str) -> Result<Option<Binding>> {
    uuid(id)?;
    let value: Option<String> = ledger
        .db
        .query_row(
            "SELECT binding FROM public_messages WHERE id=?",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    value
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
    json!({"origin":binding.origin.as_deref().unwrap_or("operator"),"origin_source":binding.origin_source.as_deref().unwrap_or("default"),
        "message_id":binding.request.message_id,"session_id":binding.request.session_id,
        "accepted_generation":binding.request.expected_generation,"provider":"codex",
        "accepted_runtime_generation":binding.native.expected_generation,
        "provider_conversation_id":binding.native.session_id,"mode":binding.request.mode,"if_busy":binding.request.if_busy,"hold_release":if binding.released_hold {json!({"from":"compaction","delivered_as":"queue","note":"held while a compaction ran; delivered with queue semantics (native queue, arrival order) after it"})} else {Value::Null},
        "delivery_semantics":"non_steering_native_queue",
        "state":if value["state"]=="unknown" {json!("delivery_unknown")} else {value["state"].clone()},
        "reason":value["reason"],"reason_detail":if value["reason"]=="unsupported_native_version" {Some("The account app-server version is not supported by this Sessionsation build. Use api observe to inspect its native_protocol version and supported_versions; no native input was attempted.")} else if value["reason"]=="compaction_hold" {Some("A compaction is queued for this session. The message is held locally (nothing is in the native queue) and is delivered once the compaction completes, fails, expires or is cancelled; cancel it or let it expire to drop it.")} else {None},"facts":{"accepted_local":true,"accepted_native":accepted,
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
        "reply":if reply.is_null() {Value::Null} else {json!({"item_id":reply["item_id"],"text":reply["text"],"phase":reply["phase"]})}})
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

/// Deliver a message a compaction held: marks its binding so it is sent with queue semantics
/// (it may not be the first to start a turn), then runs the ordinary idempotent send.
pub(super) async fn send_released(paths: &Paths, id: &str) -> anyhow::Result<Value> {
    let ledger = Ledger::open_wait(paths, wait_budget(paths, Wait::Send)).await?;
    initialize(&ledger)?;
    let mut binding = get(&ledger, id)?.ok_or(Fault("receipt_not_found"))?;
    if !binding.released_hold {
        binding.released_hold = true;
        ledger.db.execute(
            "UPDATE public_messages SET binding=? WHERE id=?",
            params![serde_json::to_string(&binding)?, id],
        )?;
    }
    let request = binding.into_request();
    drop(ledger);
    send_public(paths, request).await
}

pub async fn send_public(paths: &Paths, request: PublicRequest) -> anyhow::Result<Value> {
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
    if !delivery::timing(paths).ready.is_zero() && request.mode != PublicMode::Steer {
        // A busy ledger refuses before any catalog or native work.
        drop(Ledger::open_wait(paths, wait_budget(paths, Wait::Send)).await?);
        if let Ok(session) =
            crate::catalog::resolve(paths, &request.session_id, &request.expected_generation)
        {
            if session.provider == "codex" {
                delivery::await_ready(paths, &session.conversation_id).await;
            }
        }
    }
    let mut ledger = Ledger::open_wait(paths, wait_budget(paths, Wait::Send)).await?;
    initialize(&ledger)?;
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
            anyhow::ensure!(s.provider == "codex", "UNSUPPORTED_PROVIDER_OPERATION");
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
            let (origin, origin_source) =
                crate::caller_origin::resolve(paths, request.origin.as_deref());
            let binding = Binding {
                request,
                native,
                cancelled: false,
                released_hold: false,
                origin: Some(origin),
                origin_source: Some(origin_source.to_owned()),
                evidence_observed_at: None,
            };
            // Bind the logical request before any native mutation. A crash here is
            // provably unsent; later retries retain the exact original native target.
            super::retention::maybe_run(&mut ledger, now);
            let count: i64 =
                ledger
                    .db
                    .query_row("SELECT count(*) FROM public_messages", [], |r| r.get(0))?;
            anyhow::ensure!(count < 10000, "RECEIPT_CAPACITY_REACHED");
            ledger.db.execute(
                "INSERT INTO public_messages VALUES(?,?)",
                params![binding.request.message_id, serde_json::to_string(&binding)?],
            )?;
            binding
        };
    if binding.cancelled {
        return Ok(wrap(&binding, json!({"state":"cancelled"})));
    }
    let mut receipt = if binding.request.mode == PublicMode::Steer {
        steering::accept(&mut ledger, &binding)?
    } else {
        ledger.accept(&binding.native, crate::daemon::now())?
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
                ledger.amend(&mut receipt, |r| {
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

pub async fn receipt_public(paths: &Paths, id: &str, cancelling: bool) -> anyhow::Result<Value> {
    if super::claude_steer::exists(paths, id) {
        return super::claude_steer::receipt(paths, id, cancelling).await;
    }
    if super::headless::exists(paths, id) {
        return super::headless::receipt(paths, id, cancelling);
    }
    let mut ledger = Ledger::open_wait(paths, wait_budget(paths, Wait::Read)).await?;
    initialize(&ledger)?;
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
            ledger.db.execute(
                "UPDATE public_messages SET binding=? WHERE id=?",
                params![serde_json::to_string(&binding)?, id],
            )?;
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
                receipt.state = "cancelled".into();
                receipt.reason = None;
                ledger.save(&receipt)?;
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
        let mut ledger = Ledger::open_wait(paths, wait_budget(paths, Wait::Read)).await?;
        binding = get(&ledger, id)?.ok_or(Fault("receipt_not_found"))?;
        receipt = ledger.get(id)?.ok_or(Fault("receipt_not_found"))?;
        let status = if receipt != original {
            json!({"state":"unavailable","code":"receipt_changed_during_observation"})
        } else {
            match observation {
                Ok(Some(observed)) => {
                    steering::commit(&mut ledger, &mut receipt, observed)?;
                    binding.evidence_observed_at = Some(crate::daemon::now());
                    ledger.db.execute(
                        "UPDATE public_messages SET binding=? WHERE id=?",
                        params![serde_json::to_string(&binding)?, id],
                    )?;
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
        ledger = Ledger::open_wait(paths, wait_budget(paths, Wait::Read)).await?;
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
        ledger.db.execute(
            "UPDATE public_messages SET binding=? WHERE id=?",
            params![serde_json::to_string(&binding)?, id],
        )?;
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
        receipt.state = "rejected".into();
        receipt.reason = Some("generation_mismatch".into());
        ledger.save(receipt)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn codex_send_accepts_claude_parity_fields_and_names_unknown_ones() {
        use super::*;
        let base = json!({"api_version":1,"message_id":"m","session_id":"s","expected_generation":"g",
            "mode":"steer","text":"t","expires_at":1});
        let mut v = base.clone();
        v["origin"] = json!("agent");
        v["delivery"] = json!("auto");
        v["await_reply"] = json!(true);
        let r: PublicRequest = serde_json::from_value(v).unwrap();
        assert_eq!(r.origin.as_deref(), Some("agent"));
        assert!(check_codex_options(&r).is_ok());
        let mut native = r.clone();
        native.delivery = Some("native".into());
        let e = check_codex_options(&native).unwrap_err().to_string();
        assert!(e.starts_with("PROVIDER_DELIVERY_UNSUPPORTED"), "{e}");
        let mut bad = r.clone();
        bad.origin = Some("robot".into());
        assert!(
            check_codex_options(&bad)
                .unwrap_err()
                .to_string()
                .starts_with("INVALID_REQUEST")
        );
        let mut v = base;
        v["bogus_field"] = json!(1);
        let e = serde_json::from_value::<PublicRequest>(v)
            .unwrap_err()
            .to_string();
        assert!(e.contains("bogus_field"), "{e}");
    }
    use super::*;
    #[test]
    fn public_receipt_distinguishes_native_acceptance_and_completion() {
        let b = Binding {
            cancelled: false,
            released_hold: false,
            origin: None,
            origin_source: None,
            evidence_observed_at: None,
            request: PublicRequest {
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                steering: None,
                api_version: 1,
                message_id: uuid::Uuid::new_v4().to_string(),
                session_id: uuid::Uuid::new_v4().to_string(),
                expected_generation: "a".repeat(64),
                recovery: None,
                mode: PublicMode::Submit,
                text: "private prompt".into(),
                expires_at: 100,
            },
            native: Enqueue {
                message_id: "m".into(),
                session_id: "native".into(),
                text: "private prompt".into(),
                expires_at: 100,
                expected_generation: "b".repeat(64),
            },
        };
        let out = wrap(
            &b,
            json!({"state":"unknown","reason":"submission_uncertain"}),
        );
        assert_eq!(out["state"], "delivery_unknown");
        assert_eq!(out["facts"]["accepted_native"], false);
        assert!(!out.to_string().contains("private prompt"));
        let accepted = wrap(&b, json!({"state":"native_accepted","native_queue_id":"q"}));
        assert_eq!(accepted["facts"]["accepted_native"], true);
        assert_eq!(accepted["facts"]["reply_observed"], false);
        assert_eq!(accepted["session_id"], b.request.session_id);
        assert_ne!(accepted["session_id"], accepted["provider_conversation_id"]);
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_server};

    #[test]
    fn reading_a_pending_receipt_settles_it_when_expired_or_the_session_was_replaced() {
        let (_dir, paths, mut input, _) = fixture();
        let now = crate::daemon::now();
        input.expires_at = now + 600;
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        input.expected_generation = a.generation();
        let mut ledger = Ledger::open(&paths).unwrap();
        // Still valid and the same generation: stays pending (transient reasons keep waiting).
        let mut r = ledger.accept(&input, now).unwrap();
        r.reason = Some("migrating".into());
        settle_pending(&paths, &mut ledger, &mut r).unwrap();
        assert_eq!(
            (r.state.as_str(), r.reason.as_deref()),
            ("pending_local", Some("migrating"))
        );
        // The session behind it was replaced: terminal, nothing was sent.
        r.generation = "f".repeat(64);
        settle_pending(&paths, &mut ledger, &mut r).unwrap();
        assert_eq!(
            (r.state.as_str(), r.reason.as_deref()),
            ("rejected", Some("generation_mismatch"))
        );
        assert_eq!(
            ledger.get(&r.message_id).unwrap().unwrap().state,
            "rejected"
        );
        // Terminal receipts are left alone.
        settle_pending(&paths, &mut ledger, &mut r).unwrap();
        assert_eq!(r.state, "rejected");
        // Past its expiry a pending receipt is expired when read, never pending for ever.
        let mut late = input.clone();
        late.message_id = uuid::Uuid::new_v4().to_string();
        late.expires_at = now + 1;
        let mut r = ledger.accept(&late, now).unwrap();
        r.reason = Some("migrating".into());
        ledger
            .db
            .execute(
                "UPDATE receipts SET expiry=? WHERE id=?",
                params![now - 1, r.message_id],
            )
            .unwrap();
        settle_pending(&paths, &mut ledger, &mut r).unwrap();
        assert_eq!((r.state.as_str(), r.reason.as_deref()), ("expired", None));
    }
    fn binding(input: Enqueue) -> Binding {
        Binding {
            cancelled: false,
            released_hold: false,
            origin: None,
            origin_source: None,
            evidence_observed_at: None,
            request: PublicRequest {
                origin: None,
                delivery: None,
                await_reply: false,
                if_busy: None,
                steering: None,
                api_version: 1,
                message_id: input.message_id.clone(),
                session_id: uuid::Uuid::new_v4().to_string(),
                expected_generation: "a".repeat(64),
                recovery: None,
                mode: PublicMode::Queue,
                text: input.text.clone(),
                expires_at: input.expires_at,
            },
            native: input,
        }
    }
    #[test]
    fn only_correlated_explicit_recovery_releases_its_original_stop() {
        let (_dir, paths, input, now) = fixture();
        let mut b = binding(input.clone());
        b.request.mode = PublicMode::Resume;
        b.request.recovery = Some(super::super::recovery::Intent {
            expected_state: "stopped".into(),
            expected_previous_turn_id: None,
            expected_stop_id: Some("original-stop".into()),
            expected_runtime_generation: None,
            launch: None,
        });
        let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
        r.state = "native_accepted".into();
        let dir = paths.root.join(crate::layout::CONVERSATIONS);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{}.json", r.session_id));
        let set = |stop: &str| {
            fs::write(
                &file,
                json!({"name":null,"owner":null,"stop_intent":stop}).to_string(),
            )
            .unwrap()
        };
        set("original-stop");
        reconcile_recovered_stop(&paths, &b, &r).unwrap();
        assert_eq!(
            external::messaging_stop_intent(&paths, &r.session_id)
                .unwrap()
                .as_deref(),
            Some("original-stop")
        );
        r.state = "execution_started".into();
        r.correlation = Some(observe::Correlation {
            turn_id: "turn-1".into(),
            user_item_id: "item-1".into(),
            completed_user_item_id: None,
            observed_generation: r.generation.clone(),
        });
        reconcile_recovered_stop(&paths, &b, &r).unwrap();
        assert!(
            external::messaging_stop_intent(&paths, &r.session_id)
                .unwrap()
                .is_none()
        );
        set("newer-stop");
        reconcile_recovered_stop(&paths, &b, &r).unwrap();
        assert_eq!(
            external::messaging_stop_intent(&paths, &r.session_id)
                .unwrap()
                .as_deref(),
            Some("newer-stop")
        );
        set("original-stop");
        b.cancelled = true;
        reconcile_recovered_stop(&paths, &b, &r).unwrap();
        assert_eq!(
            external::messaging_stop_intent(&paths, &r.session_id)
                .unwrap()
                .as_deref(),
            Some("original-stop")
        );
        b.cancelled = false;
        b.request.mode = PublicMode::Queue;
        reconcile_recovered_stop(&paths, &b, &r).unwrap();
        assert_eq!(
            external::messaging_stop_intent(&paths, &r.session_id)
                .unwrap()
                .as_deref(),
            Some("original-stop")
        );
    }
    fn save_binding(paths: &Paths, b: &Binding, r: &Receipt) {
        let mut ledger = Ledger::open(paths).unwrap();
        initialize(&ledger).unwrap();
        ledger.save(r).unwrap();
        ledger
            .db
            .execute(
                "INSERT INTO public_messages VALUES(?,?)",
                params![b.request.message_id, serde_json::to_string(b).unwrap()],
            )
            .unwrap();
    }
    #[tokio::test]
    async fn passive_receipt_releases_writer_lease_and_cancellation_wins() {
        use crate::messaging::tests::{attachment_fixture, bind, scripted_server};
        let (_dir, paths, mut input, now) = fixture();
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        input.expected_generation = a.generation();
        let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
        r.state = "native_accepted".into();
        r.native_queue_id = Some("queue-1".into());
        save_binding(&paths, &binding(input.clone()), &r);
        let root = paths.root.clone();
        let runtime = paths.runtime.clone();
        let message = input.message_id.clone();
        let server = scripted_server(&a.endpoint(), move |request| {
            if request["method"] == "thread/read" {
                return Some(json!({"thread":{"id":request["params"]["threadId"],"path":null}}));
            }
            assert_eq!(request["method"], "thread/turns/list");
            // Models another caller committing cancellation while the native
            // observation is outstanding. The root-wide lease must be free.
            let p = Paths {
                root: root.clone(),
                runtime: runtime.clone(),
            };
            let mut ledger = Ledger::open(&p).unwrap();
            let mut receipt = ledger.get(&message).unwrap().unwrap();
            receipt.state = "cancelled".into();
            ledger.save(&receipt).unwrap();
            Some(
                serde_json::from_str::<Value>(include_str!(
                    "../../docs/messaging/v1.2.1/fixtures.json"
                ))
                .unwrap()["native_summary"]
                    .clone(),
            )
        })
        .await;
        let out = receipt_public(&paths, &input.message_id, false)
            .await
            .unwrap();
        assert_eq!(out["state"], "cancelled");
        assert_eq!(out["facts"]["input_recorded"], false);
        assert_eq!(
            out["observation"]["error_code"],
            "receipt_changed_during_observation"
        );
        assert_eq!(
            Ledger::open(&paths)
                .unwrap()
                .get(&input.message_id)
                .unwrap()
                .unwrap()
                .state,
            "cancelled"
        );
        server.abort();
    }
    #[tokio::test]
    async fn legacy_lock_contention_refuses_public_send_before_admission() {
        let (_dir, paths, input, now) = fixture();
        let mut legacy = Ledger::open(&paths).unwrap();
        initialize(&legacy).unwrap();
        let existing = legacy.accept(&input, now).unwrap();
        let events = legacy.events(None).unwrap();
        let mut refused = Vec::new();
        for mode in [PublicMode::Submit, PublicMode::Queue] {
            let mut b = binding(input.clone());
            b.request.message_id = uuid::Uuid::new_v4().to_string();
            b.request.mode = mode;
            let id = b.request.message_id.clone();
            let error = send_public(&paths, b.request).await.unwrap_err();
            assert_eq!(error.downcast_ref::<Fault>().unwrap().0, "adapter_busy");
            assert!(get(&legacy, &id).unwrap().is_none());
            assert!(legacy.get(&id).unwrap().is_none());
            refused.push(id);
        }
        assert_eq!(legacy.get(&input.message_id).unwrap(), Some(existing));
        assert_eq!(legacy.events(None).unwrap(), events);
        // Lock refusal precedes catalog resolution, discovery and native I/O.
        assert!(!paths.root.join(crate::layout::SESSION_CATALOG).exists());
        drop(legacy);
        for id in refused {
            let error = receipt_public(&paths, &id, false).await.unwrap_err();
            assert_eq!(
                error.downcast_ref::<Fault>().unwrap().0,
                "receipt_not_found"
            );
        }
        assert_eq!(Ledger::open(&paths).unwrap().events(None).unwrap(), events);
    }
    #[tokio::test]
    async fn busy_receipt_lookup_does_not_negate_existing_admission() {
        let (_dir, paths, input, now) = fixture();
        let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
        r.state = "native_accepted".into();
        r.native_queue_id = Some("queue-1".into());
        let b = binding(input.clone());
        save_binding(&paths, &b, &r);
        let legacy = Ledger::open(&paths).unwrap();
        let error = receipt_public(&paths, &input.message_id, false)
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<Fault>().unwrap().0, "adapter_busy");
        assert_eq!(legacy.get(&input.message_id).unwrap(), Some(r));
        assert!(get(&legacy, &input.message_id).unwrap().is_some());
        drop(legacy);
        let recovered = receipt_public(&paths, &input.message_id, false)
            .await
            .unwrap();
        assert_eq!(recovered["facts"]["accepted_native"], true);
        assert_eq!(recovered["native_queue_id"], "queue-1");
        assert_eq!(recovered["observation"]["state"], "unavailable");
    }
    #[test]
    fn projection_preserves_distinct_ids_and_filters_non_public_fields() {
        let (_dir, _paths, input, _) = fixture();
        let mut b = binding(input);
        b.evidence_observed_at = Some(100);
        let out = wrap(
            &b,
            json!({"state":"completed_with_reply","native_queue_id":"queue-1","correlation":{"turn_id":"turn-1","user_item_id":"item-41","completed_user_item_id":"item-1","observed_generation":"c".repeat(64),"raw":"SECRET"},"reply":{"item_id":"reply-1","text":"Public answer","phase":"final_answer","reasoning":"SECRET"},"observation":{"state":"cached","raw":"SECRET"},"history":"SECRET"}),
        );
        assert_eq!(out["correlation"]["original_input_item_id"], "item-41");
        assert_eq!(out["correlation"]["completion_input_item_id"], "item-1");
        assert_eq!(out["correlation"]["reply_item_id"], "reply-1");
        assert_ne!(
            out["accepted_generation"],
            out["correlation"]["observed_runtime_generation"]
        );
        assert_eq!(out["observation"]["freshness"], "stale");
        assert_eq!(out["observation"]["evidence_observed_at"], 100);
        assert!(out["observation"]["native_attempted_at"].is_null());
        assert!(!out.to_string().contains("SECRET"));
    }
    #[tokio::test]
    async fn public_projection_recovery_survives_reopen_and_expiry_without_sending() {
        let (_dir, paths, mut input, _) = fixture();
        let now = crate::daemon::now();
        input.expires_at = now - 100;
        let mut a = attachment_fixture(&paths, &input);
        bind(&paths, &mut a);
        let mid = input.message_id.clone();
        let tid = input.session_id.clone();
        let server=scripted_server(&a.endpoint(),move |req|{
            if req["method"]=="thread/read" {return Some(json!({"thread":{"id":tid,"path":null}}));}
            assert_eq!(req["method"],"thread/turns/list");assert_eq!(req["params"]["itemsView"],"summary");assert_eq!(req["params"]["threadId"],tid);
            Some(json!({"data":[{"id":"turn-1","status":"completed","itemsView":"summary","items":[{"type":"userMessage","id":"item-1","clientId":mid,"content":"SECRET"},{"type":"agentMessage","id":"reply-1","text":"Public answer","phase":"final_answer"}]}],"nextCursor":null}))
        }).await;
        input.expected_generation = a.generation();
        let b = binding(input.clone());
        let mut r = Ledger::open(&paths)
            .unwrap()
            .accept(&input, now - 200)
            .unwrap();
        r.state = "execution_started".into();
        r.native_queue_id = Some("queue-1".into());
        r.correlation = Some(observe::Correlation {
            turn_id: "turn-1".into(),
            user_item_id: "item-41".into(),
            completed_user_item_id: None,
            observed_generation: "b".repeat(64),
        });
        save_binding(&paths, &b, &r);
        let out = receipt_public(&paths, &input.message_id, false)
            .await
            .unwrap();
        assert_eq!(out["state"], "completed_with_reply");
        assert_eq!(out["correlation"]["original_input_item_id"], "item-41");
        assert_eq!(out["correlation"]["completion_input_item_id"], "item-1");
        assert_eq!(out["observation"]["state"], "observed");
        assert!(out["observation"]["evidence_observed_at"].is_i64());
        server.abort();
        let cached = receipt_public(&paths, &input.message_id, false)
            .await
            .unwrap();
        assert_eq!(cached["observation"]["state"], "cached");
        assert_eq!(
            cached["observation"]["evidence_observed_at"],
            out["observation"]["evidence_observed_at"]
        );
        assert_eq!(cached["correlation"], out["correlation"]);
        assert!(!cached.to_string().contains("SECRET"));
    }
    #[tokio::test]
    async fn observer_errors_preserve_accepted_facts_and_old_receipts_are_not_adopted() {
        let (_dir, paths, input, now) = fixture();
        let mut r = Ledger::open(&paths).unwrap().accept(&input, now).unwrap();
        r.state = "native_accepted".into();
        r.native_queue_id = Some("queue-1".into());
        Ledger::open(&paths).unwrap().save(&r).unwrap();
        assert!(
            receipt_public(&paths, &input.message_id, false)
                .await
                .is_err()
        );
        assert_eq!(
            Ledger::open(&paths)
                .unwrap()
                .get(&input.message_id)
                .unwrap(),
            Some(r.clone())
        );
        let b = binding(input.clone());
        save_binding(&paths, &b, &r);
        let out = receipt_public(&paths, &input.message_id, false)
            .await
            .unwrap();
        assert_eq!(out["state"], "native_accepted");
        assert_eq!(out["facts"]["accepted_native"], true);
        assert_eq!(out["observation"]["state"], "unavailable");
        assert!(out["observation"]["error_code"].is_string());
        assert!(out["observation"]["evidence_observed_at"].is_null());
    }
}

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
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut ledger = loop {
        match Ledger::open(paths) {
            Ok(ledger) => break ledger,
            Err(Fault("adapter_busy")) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error.into()),
        }
    };
    initialize(&ledger)?;
    let binding = if let Some(b) = get(&ledger, &request.message_id)? {
        anyhow::ensure!(b.request == request, "MESSAGE_ID_CONFLICT");
        b
    } else {
        anyhow::ensure!(
            ledger.get(&request.message_id)?.is_none(),
            "MESSAGE_ID_CONFLICT"
        );
        let (origin, origin_source) =
            crate::caller_origin::resolve(paths, request.origin.as_deref());
        let b = Binding {
            native: Enqueue {
                message_id: request.message_id.clone(),
                session_id: session.conversation_id,
                text: request.text.clone(),
                expires_at: request.expires_at,
                expected_generation: a.generation(),
            },
            request,
            cancelled: false,
            released_hold: false,
            origin: Some(origin),
            origin_source: Some(origin_source.to_owned()),
            evidence_observed_at: None,
        };
        ledger.db.execute(
            "INSERT INTO public_messages VALUES(?,?)",
            params![b.request.message_id, serde_json::to_string(&b)?],
        )?;
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
