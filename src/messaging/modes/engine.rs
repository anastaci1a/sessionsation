//! Durable, deterministic fixture adapter. No OS process or native socket access.
use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Snapshot {
    session_id: String,
    generation: String,
    capability_id: String,
    build: String,
    state: String,
    turn_id: Option<String>,
    observed_at: i64,
    capability_expires_at: i64,
    #[serde(default)]
    modal: bool,
    /// Hypothetical mock guarantee, absent from installed turn/start.
    #[serde(default)]
    atomic_idle_start: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(super) struct Receipt {
    request_id: String,
    session_id: String,
    requested_mode: Mode,
    effective_action: Option<String>,
    state: String,
    reason: Option<String>,
    facts: Vec<String>,
    turn_id: Option<String>,
    user_item_id: Option<String>,
    reply: Option<Value>,
}
struct Journal {
    db: Connection,
    _lock: fs::File,
}
impl Journal {
    fn open(root: &std::path::Path) -> Result<Self> {
        private_dir(root).map_err(|_| Fault("storage_error"))?;
        let lock = lock_file(&root.join("modes.lock")).map_err(|_| Fault("storage_error"))?;
        lock.try_lock_exclusive()
            .map_err(|_| Fault("adapter_busy"))?;
        let db = Connection::open(root.join("fixture.db"))?;
        db.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS requests(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE,hash TEXT,request TEXT,receipt TEXT);")?;
        // Holding the exclusive journal lock proves any prior owner is gone.
        // Intent may have crossed the boundary: never reclaim it for dispatch.
        db.execute("UPDATE requests SET receipt=json_set(receipt,'$.state','delivery_unknown','$.reason','restarted_after_dispatch_intent') WHERE json_extract(receipt,'$.state')='dispatch_intent'", [])?;
        Ok(Self { db, _lock: lock })
    }
    fn get(&self, id: &str) -> Result<Option<Receipt>> {
        let text: Option<String> = self
            .db
            .query_row("SELECT receipt FROM requests WHERE id=?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        text.map(|s| serde_json::from_str(&s).map_err(|_| Fault("storage_error")))
            .transpose()
    }
    fn save(&self, r: &Receipt) -> Result<()> {
        self.db.execute(
            "UPDATE requests SET receipt=? WHERE id=?",
            params![
                serde_json::to_string(r).map_err(|_| Fault("storage_error"))?,
                r.request_id
            ],
        )?;
        Ok(())
    }
    fn accept(&self, r: &Request, now: i64) -> Result<Receipt> {
        r.validate()?;
        let hash = r.fingerprint()?;
        if let Some(old) = self.get(&r.request_id)? {
            let stored: String = self.db.query_row(
                "SELECT hash FROM requests WHERE id=?",
                [&r.request_id],
                |r| r.get(0),
            )?;
            return if stored == hash {
                Ok(old)
            } else {
                Err(Fault("idempotency_conflict"))
            };
        }
        if r.created_at > now + 30 || r.expires_at <= now {
            return Err(Fault("expired_or_future_request"));
        }
        let count: i64 = self
            .db
            .query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?;
        if count >= 1000 {
            return Err(Fault("backpressure"));
        }
        let receipt = Receipt {
            request_id: r.request_id.clone(),
            session_id: r.session_id.clone(),
            requested_mode: r.mode.clone(),
            effective_action: None,
            state: "adapter_accepted".into(),
            reason: None,
            facts: vec!["adapter_accepted".into()],
            turn_id: None,
            user_item_id: None,
            reply: None,
        };
        self.db.execute(
            "INSERT INTO requests(id,hash,request,receipt) VALUES(?,?,?,?)",
            params![
                r.request_id,
                hash,
                serde_json::to_string(r).map_err(|_| Fault("storage_error"))?,
                serde_json::to_string(&receipt).map_err(|_| Fault("storage_error"))?
            ],
        )?;
        Ok(receipt)
    }
    fn dispatch(
        &self,
        r: &Request,
        now: i64,
        s: &Snapshot,
        native_result: &str,
    ) -> Result<(Receipt, Option<Value>)> {
        let mut receipt = self.get(&r.request_id)?.ok_or(Fault("receipt_not_found"))?;
        if !matches!(receipt.state.as_str(), "adapter_accepted" | "waiting") {
            return Ok((receipt, None));
        }
        let reject = |mut receipt: Receipt, code: &str| -> Result<(Receipt, Option<Value>)> {
            receipt.state = "reconciliation_required".into();
            receipt.reason = Some(code.into());
            self.save(&receipt)?;
            Ok((receipt, None))
        };
        if now >= r.expires_at {
            receipt.state = "expired".into();
            self.save(&receipt)?;
            return Ok((receipt, None));
        }
        if s.session_id != r.session_id {
            return reject(receipt, "target_changed");
        }
        if s.generation != r.expected_generation {
            return reject(receipt, "generation_mismatch");
        }
        if s.build != "0.163.0"
            || s.capability_id != r.capability_id
            || now >= s.capability_expires_at
        {
            return reject(receipt, "capability_changed");
        }
        if s.observed_at < 0 || s.observed_at > now || now - s.observed_at > 5 || s.modal {
            return reject(receipt, "state_unverified");
        }
        if s.state == "migrating" {
            receipt.state = "waiting".into();
            receipt.reason = Some("migration_in_progress".into());
            self.save(&receipt)?;
            return Ok((receipt, None));
        }
        if !matches!(s.state.as_str(), "idle" | "busy") {
            return reject(receipt, "target_ineligible");
        }
        self.db.execute("UPDATE requests SET receipt=json_set(receipt,'$.state','expired') WHERE json_extract(request,'$.expires_at')<=? AND json_extract(receipt,'$.state') IN ('adapter_accepted','waiting')", [now])?;
        let older:i64=self.db.query_row("SELECT count(*) FROM requests WHERE seq<(SELECT seq FROM requests WHERE id=?) AND json_extract(receipt,'$.session_id')=? AND json_extract(receipt,'$.state') NOT IN ('cancelled','expired','turn_completed','completed_with_reply')",params![r.request_id,r.session_id],|r|r.get(0))?;
        if r.mode == Mode::Queue && (s.state == "busy" || older > 0) {
            receipt.state = "waiting".into();
            self.save(&receipt)?;
            return Ok((receipt, None));
        }
        let (method, action) = match r.mode {
            Mode::Queue => ("turn/start", "start"), // fixture scheduler, not a new native queue implementation
            Mode::Submit if s.state == "busy" => {
                if r.expected_turn_id.is_none() || r.expected_turn_id != s.turn_id {
                    return reject(receipt, "turn_conflict");
                }
                ("turn/steer", "steer")
            }
            Mode::Submit => {
                if r.expected_turn_id.is_some() {
                    return reject(receipt, "turn_conflict");
                }
                ("turn/start", "start")
            }
        };
        if r.mode == Mode::Queue && action == "start" && !s.atomic_idle_start {
            return reject(receipt, "unsupported_idle_submit");
        }
        // Record the expected busy turn before possible delivery, including lost acks.
        if action == "steer" {
            receipt.turn_id = r.expected_turn_id.clone();
        }
        // Persist before the mock boundary. On restart dispatch_intent never retries.
        receipt.state = "dispatch_intent".into();
        self.save(&receipt)?;
        if native_result == "crash_before_ack" {
            return Ok((receipt, None));
        }
        let mut params = json!({"threadId":s.session_id,"clientUserMessageId":r.request_id,"input":[{"type":"text","text":r.text,"text_elements":[]}]});
        if method == "turn/steer" {
            params["expectedTurnId"] = json!(r.expected_turn_id);
        }
        let call = json!({"method":method,"params":params});
        match native_result {
            "ack" => {
                receipt.state = "native_accepted".into();
                receipt.effective_action =
                    if r.mode == Mode::Submit && action == "start" && !s.atomic_idle_start {
                        None
                    } else {
                        Some(action.into())
                    };
                receipt.turn_id = if action == "steer" {
                    s.turn_id.clone()
                } else {
                    Some(format!("fixture-turn-{}", r.request_id))
                };
                receipt.facts.push("native_accepted".into());
            }
            "turn_conflict" => {
                receipt.state = "reconciliation_required".into();
                receipt.reason = Some("turn_conflict".into());
            }
            _ => {
                receipt.state = "delivery_unknown".into();
                receipt.reason = Some("possible_native_acceptance_never_replay".into());
            }
        }
        self.save(&receipt)?;
        Ok((receipt, Some(call)))
    }
    #[allow(clippy::too_many_arguments)]
    fn observe(
        &self,
        id: &str,
        session: &str,
        turn: &str,
        client: &str,
        fact: &str,
        item: &str,
        text: Option<String>,
    ) -> Result<Receipt> {
        let mut r = self.get(id)?.ok_or(Fault("receipt_not_found"))?;
        if r.session_id != session
            || client != id
            || r.turn_id.as_deref().is_some_and(|t| t != turn)
            || turn.is_empty()
            || turn.len() > 256
            || item.len() > 256
        {
            return Err(Fault("correlation_changed"));
        }
        if !matches!(
            r.state.as_str(),
            "native_accepted"
                | "delivery_unknown"
                | "dispatch_intent"
                | "input_recorded"
                | "consumed"
                | "reply_observed"
                | "turn_completed"
                | "completed_with_reply"
        ) {
            return Err(Fault("invalid_observation"));
        }
        let stored: String =
            self.db
                .query_row("SELECT request FROM requests WHERE id=?", [id], |row| {
                    row.get(0)
                })?;
        let request: Request = serde_json::from_str(&stored).map_err(|_| Fault("storage_error"))?;
        if request.mode == Mode::Submit
            && request.expected_turn_id.is_some()
            && fact == "reply_observed"
        {
            // No inspected native field links a reply to a particular steer.
            // Even consumption and same-turn output do not establish causality.
            return Err(Fault("unsupported_steer_reply_correlation"));
        }
        match fact {
            "input_recorded" | "native_input_consumed" => {
                if r.user_item_id.as_deref().is_some_and(|old| old != item) {
                    return Err(Fault("correlation_changed"));
                }
                r.user_item_id = Some(item.into());
                r.turn_id = Some(turn.into());
            }
            "turn_completed" => {
                if r.turn_id.is_none() {
                    return Err(Fault("uncorrelated"));
                }
            }
            "reply_observed" => {
                if r.turn_id.is_none() {
                    return Err(Fault("uncorrelated"));
                }
                let text = text.ok_or(Fault("invalid_observation"))?;
                if text.is_empty() || text.len() > 65536 {
                    return Err(Fault("invalid_observation"));
                }
                let reply = json!({"item_id":item,"text":text});
                if r.reply.as_ref().is_some_and(|old| old != &reply) {
                    return Err(Fault("correlation_changed"));
                }
                r.reply = Some(reply);
            }
            _ => return Err(Fault("unsupported_fact")),
        }
        if !r.facts.iter().any(|f| f == fact) {
            r.facts.push(fact.into());
        }
        let completed = r.facts.iter().any(|f| f == "turn_completed");
        r.state = if completed && r.reply.is_some() {
            "completed_with_reply"
        } else if completed {
            "turn_completed"
        } else if r.reply.is_some() {
            "reply_observed"
        } else if r.facts.iter().any(|f| f == "native_input_consumed") {
            "consumed"
        } else {
            "input_recorded"
        }
        .into();
        self.save(&r)?;
        Ok(r)
    }
    fn cancel(&self, id: &str) -> Result<Receipt> {
        let mut r = self.get(id)?.ok_or(Fault("receipt_not_found"))?;
        if !matches!(r.state.as_str(), "adapter_accepted" | "waiting") {
            return Err(Fault("already_submitted_or_unknown"));
        }
        r.state = "cancelled".into();
        r.facts.push("cancelled".into());
        self.save(&r)?;
        Ok(r)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scenario {
    now: i64,
    authenticated: bool,
    csrf_valid: bool,
    origin: String,
    #[serde(default)]
    sending_enabled: bool,
    steps: Vec<Step>,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Accept {
        request: Request,
    },
    Dispatch {
        request_id: String,
        snapshot: Snapshot,
        native_result: String,
    },
    Observe {
        request_id: String,
        session_id: String,
        turn_id: String,
        client_id: String,
        fact: String,
        item_id: String,
        text: Option<String>,
    },
    NativeObservation {
        request_id: String,
        notification: String,
    },
    Restart,
    Status {
        request_id: String,
    },
    Cancel {
        request_id: String,
    },
    CheckIn {
        snapshot: Snapshot,
        last_sent: Option<i64>,
        user_enabled: bool,
        trigger: String,
    },
}
fn check_in(s: &Snapshot, now: i64, last: Option<i64>, enabled: bool, trigger: &str) -> Value {
    let eligible = enabled
        && trigger == "user_timer"
        && s.observed_at >= 0
        && s.observed_at <= now
        && now - s.observed_at <= 5
        && !s.modal
        && last.is_none_or(|t| t >= 0 && t <= now && now - t >= 1800)
        && matches!(s.state.as_str(), "idle" | "busy");
    json!({"eligible_in_fixture":eligible,"requested_mode":if eligible {Some(if s.state=="idle"{"submit"}else{"queue"})}else{None},"native_delivery":false,"reason":if eligible{"staged_approval_required"}else{"suppressed"}})
}
pub(super) fn fixture(s: Scenario) -> Result<Value> {
    if s.now < 0 || s.now > i64::MAX - 3600 || s.steps.len() > 100 {
        return Err(Fault("input_too_large"));
    }
    if !s.authenticated {
        return Err(Fault("unauthenticated"));
    }
    if s.origin != ORIGIN {
        return Err(Fault("origin_rejected"));
    }
    if !s.csrf_valid {
        return Err(Fault("csrf_rejected"));
    }
    let tmp = tempfile::tempdir()?;
    let mut journal = Some(Journal::open(tmp.path())?);
    let mut results = Vec::new();
    for step in s.steps {
        let j = journal.as_ref().ok_or(Fault("storage_error"))?;
        let outcome: Result<Value> = (|| match step {
            Step::Accept { request } => {
                if !s.sending_enabled {
                    return Err(Fault("native_sending_disabled"));
                }
                Ok(json!(j.accept(&request, s.now)?))
            }
            Step::Dispatch {
                request_id,
                snapshot,
                native_result,
            } => {
                if !s.sending_enabled {
                    return Err(Fault("native_sending_disabled"));
                }
                let raw: String = j.db.query_row(
                    "SELECT request FROM requests WHERE id=?",
                    [&request_id],
                    |r| r.get(0),
                )?;
                let request: Request =
                    serde_json::from_str(&raw).map_err(|_| Fault("storage_error"))?;
                let (receipt, call) = j.dispatch(&request, s.now, &snapshot, &native_result)?;
                Ok(json!({"receipt":receipt,"mock_call":call}))
            }
            Step::Observe {
                request_id,
                session_id,
                turn_id,
                client_id,
                fact,
                item_id,
                text,
            } => Ok(json!(j.observe(
                &request_id,
                &session_id,
                &turn_id,
                &client_id,
                &fact,
                &item_id,
                text
            )?)),
            Step::NativeObservation {
                request_id,
                notification,
            } => {
                if notification.len() > 65536 {
                    return Err(Fault("input_too_large"));
                }
                let notification = serde_json::value::RawValue::from_string(notification)
                    .map_err(|_| Fault("malformed_public_event"))?;
                let r = j.get(&request_id)?.ok_or(Fault("receipt_not_found"))?;
                let turn = r.turn_id.as_deref().ok_or(Fault("uncorrelated"))?;
                let item = super::native_evidence::recorded_input(
                    &notification,
                    &r.session_id,
                    turn,
                    &request_id,
                )?;
                Ok(json!(j.observe(
                    &request_id,
                    &r.session_id,
                    turn,
                    &request_id,
                    "input_recorded",
                    &item,
                    None
                )?))
            }
            Step::Restart => Ok(json!({"restart":true})),
            Step::Status { request_id } => Ok(json!(
                j.get(&request_id)?.ok_or(Fault("receipt_not_found"))?
            )),
            Step::Cancel { request_id } => Ok(json!(j.cancel(&request_id)?)),
            Step::CheckIn {
                snapshot,
                last_sent,
                user_enabled,
                trigger,
            } => Ok(check_in(
                &snapshot,
                s.now,
                last_sent,
                user_enabled,
                &trigger,
            )),
        })();
        let restart = outcome.as_ref().is_ok_and(|v| v["restart"] == true);
        results.push(match outcome {
            Ok(v) => json!({"ok":true,"result":v}),
            Err(e) => json!({"ok":false,"error":{"code":e.0}}),
        });
        if restart {
            // The fixture simulates owner exit. Concurrent process-spawning tests
            // can transiently inherit a descriptor between fork and exec; release
            // this fixture lock explicitly before simulating the new owner.
            if let Some(owner) = journal.as_ref() {
                let _ = FileExt::unlock(&owner._lock);
            }
            drop(journal.take());
            journal = Some(Journal::open(tmp.path())?);
        }
    }
    Ok(json!({"isolation":"temporary_sqlite_no_native_io","results":results}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn req() -> Request {
        serde_json::from_value(
            serde_json::from_str::<Value>(include_str!(
                "../../../tests/fixtures/legacy-modes.json"
            ))
            .unwrap()["request"]
                .clone(),
        )
        .unwrap()
    }
    fn snap() -> Snapshot {
        serde_json::from_value(
            serde_json::from_str::<Value>(include_str!(
                "../../../tests/fixtures/legacy-modes.json"
            ))
            .unwrap()["snapshot"]
                .clone(),
        )
        .unwrap()
    }
    #[test]
    fn exclusive_claim_restart_expiry_and_cancel_are_durable() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        assert_eq!(Journal::open(d.path()).err().unwrap().0, "adapter_busy");
        let r = req();
        j.accept(&r, r.created_at).unwrap();
        let (out, call) = j.dispatch(&r, r.expires_at, &snap(), "ack").unwrap();
        assert_eq!(out.state, "expired");
        assert!(call.is_none());
        drop(j);
        let j = Journal::open(d.path()).unwrap();
        assert_eq!(j.accept(&r, r.expires_at + 100).unwrap().state, "expired");
        let mut r = r;
        r.request_id = uuid::Uuid::new_v4().to_string();
        j.accept(&r, r.created_at).unwrap();
        assert_eq!(j.cancel(&r.request_id).unwrap().state, "cancelled");
        assert!(
            j.dispatch(&r, r.created_at, &snap(), "ack")
                .unwrap()
                .1
                .is_none()
        );
    }
    #[test]
    fn bounded_backlog_does_not_evict_idempotency_keys() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        let mut r = req();
        for _ in 0..1000 {
            r.request_id = uuid::Uuid::new_v4().to_string();
            j.accept(&r, r.created_at).unwrap();
        }
        assert!(j.accept(&r, r.created_at).is_ok());
        r.request_id = uuid::Uuid::new_v4().to_string();
        assert_eq!(j.accept(&r, r.created_at).unwrap_err().0, "backpressure");
    }
    #[test]
    fn migration_waits_then_rejects_changed_generation_and_other_session_replies() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        let r = req();
        j.accept(&r, r.created_at).unwrap();
        let mut s = snap();
        s.state = "migrating".into();
        assert_eq!(
            j.dispatch(&r, r.created_at, &s, "ack").unwrap().0.state,
            "waiting"
        );
        s.state = "idle".into();
        s.generation = "b".repeat(64);
        let (out, call) = j.dispatch(&r, r.created_at, &s, "ack").unwrap();
        assert_eq!(out.reason.as_deref(), Some("generation_mismatch"));
        assert!(call.is_none());
        assert_eq!(
            j.observe(
                &r.request_id,
                "different-session",
                "turn-a",
                &r.request_id,
                "reply_observed",
                "public-1",
                Some("reply".into())
            )
            .unwrap_err()
            .0,
            "correlation_changed"
        );
    }
    #[test]
    fn expired_unsent_head_does_not_block_later_queue_request() {
        let d = tempfile::tempdir().unwrap();
        let j = Journal::open(d.path()).unwrap();
        let mut old = req();
        old.mode = Mode::Queue;
        old.expires_at = old.created_at + 1;
        j.accept(&old, old.created_at).unwrap();
        let mut next = req();
        next.mode = Mode::Queue;
        next.request_id = uuid::Uuid::new_v4().to_string();
        j.accept(&next, next.created_at).unwrap();
        let (out, call) = j
            .dispatch(&next, next.created_at + 2, &snap(), "ack")
            .unwrap();
        assert_eq!(out.state, "native_accepted");
        assert!(call.is_some());
        assert_eq!(j.get(&old.request_id).unwrap().unwrap().state, "expired");
    }
}
