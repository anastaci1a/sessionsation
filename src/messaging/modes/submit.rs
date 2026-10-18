//! Busy Submit with native expected-turn guard; acceptance never means consumption.
use super::*;
#[derive(Deserialize)]
struct Page {
    data: Vec<Turn>,
    #[serde(rename = "nextCursor")]
    next: Option<String>,
}
#[derive(Deserialize)]
struct Turn {
    id: String,
    status: String,
}

pub(super) async fn check(paths: &Paths, id: &str, expected: Option<&str>) -> Result<Value> {
    uuid(id)?;
    if expected.is_some_and(|t| t.is_empty() || t.len() > 256 || t.chars().any(char::is_control)) {
        return Err(Fault("invalid_precondition"));
    }
    let _guard = mutation_lock(paths, id)?;
    let selected = attachment(paths, id).map_err(|e| {
        if e.0 == "unavailable" {
            Fault("target_unavailable")
        } else {
            e
        }
    })?;
    let generation = selected.generation();
    let mut control = selected.connect().await?;
    let before = activity(&mut control, id).await?;
    let page: Page = control
        .request(
            "thread/turns/list",
            json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":50}),
        )
        .await?;
    if page.data.len() > 50 {
        return Err(Fault("malformed_summary"));
    }
    let mut active = page.data.iter().filter(|t| t.status == "inProgress");
    let turn = active.next().map(|t| t.id.as_str());
    if active.next().is_some() {
        return Err(Fault("ambiguous_native_correlation"));
    }
    if turn.is_some_and(|t| t.is_empty() || t.len() > 256 || t.chars().any(char::is_control)) {
        return Err(Fault("malformed_summary"));
    }
    let after = activity(&mut control, id).await?;
    if before != after || attachment(paths, id)?.generation() != generation {
        return Err(Fault("state_changed"));
    }
    let action = match (after, turn) {
        ("idle", None) if expected.is_none() => "start",
        ("busy", Some(turn)) if expected.is_none() || expected == Some(turn) => "steer",
        ("busy", None) if page.next.is_some() => return Err(Fault("turn_evidence_incomplete")),
        ("idle" | "busy", _) => return Err(Fault("turn_conflict")),
        _ => return Err(Fault("state_unverified")),
    };
    let explicit_resume = page
        .data
        .first()
        .is_some_and(|t| matches!(t.status.as_str(), "interrupted" | "failed"))
        || external::messaging_stop_intent(paths, id)
            .map_err(|_| Fault("discovery_unavailable"))?
            .is_some();
    Ok(
        json!({"previous_turn_id":page.data.first().map(|t|&t.id),"session_id":id,"generation":generation,"observed_at":crate::daemon::now(),"active_turn_id":turn,"candidate_action":action,"effective_action":null,"dispatch_supported":!explicit_resume && queue::sending_enabled(paths)?,"reason":if explicit_resume {"explicit_resume_required"}else if action=="start" {"idle_checked_start_may_steer_if_raced"}else{"consumption_and_reply_unknown"},"preflight_is_not_native_reservation":true,"input_consumption":"unsupported","reply_correlation":"unsupported"}),
    )
}

pub(super) fn capability_id(generation: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{VERSION}:submit:0.163.0:{generation}").as_bytes())
    )
}

pub(super) fn wrap(delivery: Value) -> Value {
    let acknowledged = !delivery["submit"]["acknowledged_turn_id"].is_null();
    let mut out = json!({"request_id":delivery["message_id"],"session_id":delivery["session_id"],"generation":delivery["generation"],"requested_mode":"submit","effective_action":if acknowledged {delivery["submit"]["action"].as_str().or(Some("steer"))} else {None},"state":if acknowledged {"native_accepted"} else if delivery["state"]=="rejected" {"rejected"} else {"delivery_unknown"},"reason":delivery["reason"],"expected_turn_id":delivery["submit"]["expected_turn_id"],"acknowledged_turn_id":delivery["submit"]["acknowledged_turn_id"],"facts":{"adapter_accepted":true,"native_input_accepted":acknowledged,"input_consumed":"unknown","turn_completed":"unknown","reply_observed":false},"reply":null,"resume":delivery["submit"]["resume"].as_bool().unwrap_or(false),"reconciliation":if delivery["submit"]["action"]=="start_or_steer" {"unsupported_submit_reply_evidence"} else {"unsupported_steer_evidence"}});
    if delivery["submit"]["action"] == "start_or_steer" {
        out["expected_previous_turn_id"] = if delivery["submit"]["expected_turn_id"] == "" {
            Value::Null
        } else {
            delivery["submit"]["expected_turn_id"].clone()
        };
        out["expected_turn_id"] = Value::Null;
        out["effective_action"] = Value::Null;
        out["native_operation"] = json!("turn/start");
        out["dispatch_semantics"] = json!("idle_checked_start_or_steer");
        out["idle_race"] = json!("may_steer_a_concurrently_started_turn");
    }
    out
}

pub(super) async fn dispatch(
    paths: &Paths,
    ledger: &mut Ledger,
    request: Request,
) -> Result<Value> {
    // The caller holds the shared durable dedup/dispatch lock, including retries.
    let selected = attachment(paths, &request.session_id).map_err(|e| {
        if e.0 == "unavailable" {
            Fault("target_unavailable")
        } else {
            e
        }
    })?;
    if selected.generation() != request.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    if !queue::sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    if request.expected_turn_id.is_none() {
        return dispatch_idle(paths, ledger, request).await;
    }
    if external::messaging_stop_intent(paths, &request.session_id)
        .map_err(|_| Fault("discovery_unavailable"))?
        .is_some()
    {
        return Err(Fault("explicit_resume_required"));
    }
    // No start-or-steer fallback: an idle request must explicitly remain unsupported.
    let assessment = check(
        paths,
        &request.session_id,
        request.expected_turn_id.as_deref(),
    )
    .await?;
    if assessment["candidate_action"] != "steer" {
        return Err(Fault("turn_conflict"));
    }
    if request.capability_id != capability_id(&request.expected_generation) {
        return Err(Fault("capability_changed"));
    }
    let turn = request
        .expected_turn_id
        .as_deref()
        .ok_or(Fault("invalid_precondition"))?;
    let _movement = mutation_lock(paths, &request.session_id)?;
    let selected = attachment(paths, &request.session_id)?;
    if selected.generation() != request.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    let mut control = selected.connect().await?;
    if external::messaging_stop_intent(paths, &request.session_id)
        .map_err(|_| Fault("discovery_unavailable"))?
        .is_some()
    {
        return Err(Fault("explicit_resume_required"));
    }
    let observed = session_status::observe(&mut control, &request.session_id).await?;
    if observed.reason.is_some() {
        return Err(Fault("state_unverified"));
    }
    if observed.state != "working" {
        return Err(Fault("turn_conflict"));
    }
    if attachment(paths, &request.session_id)?.generation() != request.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    if !queue::sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    let now = crate::daemon::now();
    if request.created_at > now + 30 || request.expires_at <= now {
        return Err(Fault("expired_or_future_request"));
    }
    let hash = request.fingerprint()?;
    let input = Enqueue {
        message_id: request.request_id.clone(),
        session_id: request.session_id.clone(),
        text: request.text.clone(),
        expires_at: request.expires_at,
        expected_generation: request.expected_generation.clone(),
    };
    // Persist expected native turn AND unknown intent before any native mutation.
    let evidence = SubmitEvidence {
        expected_turn_id: turn.into(),
        acknowledged_turn_id: None,
        action: None,
        resume: false,
    };
    let mut receipt = ledger.accept_bound(&input, now, Some((&hash, "submit", Some(evidence))))?;
    match control
        .steer(
            &request.session_id,
            &request.request_id,
            turn,
            &request.text,
        )
        .await
    {
        Ok(turn) => {
            receipt
                .submit
                .as_mut()
                .ok_or(Fault("storage_error"))?
                .acknowledged_turn_id = Some(turn);
            receipt.state = "native_accepted".into();
            receipt.reason = Some("consumption_and_reply_unknown".into());
        }
        Err(error) => {
            // A generic native error, malformed ack or disconnect is not proof of no effect.
            // No retry, start, queue fallback, private scan, or manufactured reply.
            receipt.reason = Some(error.0.into());
        }
    }
    ledger.save(&receipt)?;
    Ok(wrap(json!(receipt)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn crashed_submit_intent_is_atomic_and_never_reclaimed_or_misread_as_queue() {
        let (_tmp, paths, input, now) = super::super::super::tests::fixture();
        let request = Request {
            request_id: input.message_id.clone(),
            session_id: input.session_id.clone(),
            mode: Mode::Submit,
            text: input.text.clone(),
            expected_generation: input.expected_generation.clone(),
            expected_turn_id: Some("turn-a".into()),
            expected_previous_turn_id: None,
            expected_stop_id: None,
            resume: false,
            capability_id: capability_id(&input.expected_generation),
            created_at: now,
            expires_at: input.expires_at,
        };
        {
            let mut ledger = Ledger::open(&paths).unwrap();
            let receipt = ledger
                .accept_bound(
                    &input,
                    now,
                    Some((
                        &request.fingerprint().unwrap(),
                        "submit",
                        Some(SubmitEvidence {
                            expected_turn_id: "turn-a".into(),
                            acknowledged_turn_id: None,
                            action: None,
                            resume: false,
                        }),
                    )),
                )
                .unwrap();
            assert_eq!(receipt.state, "unknown");
            assert_eq!(receipt.submit.unwrap().expected_turn_id, "turn-a");
            assert!(Ledger::open(&paths).is_err());
        }
        // No target exists: retry/status must use only persisted intent.
        let repeated = queue::request(&paths, request.clone()).await.unwrap();
        assert_eq!(repeated["state"], "delivery_unknown");
        assert_eq!(repeated["expected_turn_id"], "turn-a");
        assert_eq!(
            queue::status(&paths, &request.request_id).await.unwrap(),
            repeated
        );
        let mut ledger = Ledger::open(&paths).unwrap();
        assert_eq!(
            ledger.accept(&input, now).unwrap_err().0,
            "message_id_conflict"
        );
        drop(ledger);
        let mut changed = request;
        changed.mode = Mode::Queue;
        changed.expected_turn_id = None;
        assert_eq!(
            queue::request(&paths, changed).await.unwrap_err().0,
            "idempotency_conflict"
        );
    }
}

#[cfg(test)]
#[test]
fn published_submit_receipt_matches_actual_projection() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../docs/messaging/modes-v1/submit-fixtures.json"
    ))
    .unwrap();
    assert_eq!(wrap(fixture["stored_receipt"].clone()), fixture["result"]);
    let request: Request = serde_json::from_value(fixture["request"].clone()).unwrap();
    assert_eq!(
        request.capability_id,
        capability_id(&request.expected_generation)
    );
}

/// Stock native start-or-steer after an idle/terminal preflight. The owner
/// explicitly accepts concurrent-start steering; no atomic idle guarantee.
async fn dispatch_idle(paths: &Paths, ledger: &mut Ledger, request: Request) -> Result<Value> {
    let _movement = mutation_lock(paths, &request.session_id)?;
    let selected = attachment(paths, &request.session_id)?;
    if selected.generation() != request.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    if request.capability_id != capability_id(&request.expected_generation) {
        return Err(Fault("capability_changed"));
    }
    let mut control = selected.connect().await?;
    let observed = session_status::observe(&mut control, &request.session_id).await?;
    if observed.state == "working" {
        return Err(Fault("invalid_precondition"));
    }
    if !observed.submit_supported {
        return Err(Fault("state_unverified"));
    }
    if observed.reason.is_some() {
        return Err(Fault("state_unverified"));
    }
    if observed.previous != request.expected_previous_turn_id {
        return Err(Fault("turn_conflict"));
    }
    let stop = external::messaging_stop_intent(paths, &request.session_id)
        .map_err(|_| Fault("discovery_unavailable"))?;
    if stop != request.expected_stop_id {
        return Err(Fault("stop_intent_changed"));
    }
    if stop.is_some() && !request.resume {
        return Err(Fault("explicit_resume_required"));
    }
    match (observed.state.as_str(), request.resume) {
        ("idle", true) if stop.is_some() => (),
        ("idle", false) | ("interrupted" | "errored", true) => (),
        ("interrupted" | "errored", false) => return Err(Fault("explicit_resume_required")),
        _ => return Err(Fault("turn_conflict")),
    }
    if attachment(paths, &request.session_id)?.generation() != request.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    if !queue::sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    let now = crate::daemon::now();
    if request.created_at > now + 30 || request.expires_at <= now {
        return Err(Fault("expired_or_future_request"));
    }
    let hash = request.fingerprint()?;
    let input = Enqueue {
        message_id: request.request_id.clone(),
        session_id: request.session_id.clone(),
        text: request.text.clone(),
        expires_at: request.expires_at,
        expected_generation: request.expected_generation.clone(),
    };
    let evidence = SubmitEvidence {
        expected_turn_id: request
            .expected_previous_turn_id
            .clone()
            .unwrap_or_default(),
        acknowledged_turn_id: None,
        action: Some("start_or_steer".into()),
        resume: request.resume,
    };
    let mut receipt = ledger.accept_bound(&input, now, Some((&hash, "submit", Some(evidence))))?;
    match control
        .start_or_steer(&request.session_id, &request.request_id, &request.text)
        .await
    {
        Ok(turn) => {
            receipt
                .submit
                .as_mut()
                .ok_or(Fault("storage_error"))?
                .acknowledged_turn_id = Some(turn);
            receipt.state = "native_accepted".into();
            receipt.reason = Some("consumption_and_reply_unknown".into());
            // The movement lock also serializes manual stop requests.
            if request.resume {
                external::messaging_clear_stop_intent(paths, &request.session_id)
                    .map_err(|_| Fault("local_io_error"))?;
            }
        }
        Err(error) => receipt.reason = Some(error.0.into()),
    }
    ledger.save(&receipt)?;
    Ok(wrap(json!(receipt)))
}
