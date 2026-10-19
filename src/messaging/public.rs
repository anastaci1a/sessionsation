//! Stable backend-facing Submit/Queue requests using the existing delivery ledger.
//! Both modes use the native non-steering queue. Submit additionally requires an
//! idle preflight; a concurrent turn can defer delivery but can never be steered.
use super::*;
use rusqlite::OptionalExtension;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicMode {
    Submit,
    Queue,
    Resume,
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
}

#[derive(Serialize, Deserialize)]
pub(super) struct Binding {
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    evidence_observed_at: Option<i64>,
    request: PublicRequest,
    native: Enqueue,
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
    json!({"message_id":binding.request.message_id,"session_id":binding.request.session_id,
        "accepted_generation":binding.request.expected_generation,"provider":"codex",
        "accepted_runtime_generation":binding.native.expected_generation,
        "provider_conversation_id":binding.native.session_id,"mode":binding.request.mode,
        "delivery_semantics":"non_steering_native_queue",
        "state":if value["state"]=="unknown" {json!("delivery_unknown")} else {value["state"].clone()},
        "reason":value["reason"],"facts":{"accepted_local":true,"accepted_native":accepted,
            "input_recorded":!value["correlation"].is_null(),"input_consumed":"unknown",
            "reply_observed":!reply.is_null()},
        "native_queue_id":value["native_queue_id"],
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

pub async fn send_public(paths: &Paths, request: PublicRequest) -> anyhow::Result<Value> {
    anyhow::ensure!(
        request.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    if request.mode == PublicMode::Resume {
        return super::recovery::send(paths, request).await;
    }
    anyhow::ensure!(
        request.recovery.is_none(),
        "RECOVERY_ONLY_VALID_WITH_RESUME"
    );
    uuid(&request.message_id)?;
    anyhow::ensure!(
        !super::recovery::exists(paths, &request.message_id),
        "MESSAGE_ID_CONFLICT"
    );
    uuid(&request.session_id)?;
    let mut ledger = Ledger::open(paths)?;
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
            let binding = Binding {
                request,
                native,
                cancelled: false,
                evidence_observed_at: None,
            };
            // Bind the logical request before any native mutation. A crash here is
            // provably unsent; later retries retain the exact original native target.
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
    let mut receipt = ledger.accept(&binding.native, crate::daemon::now())?;
    ledger.expire(&mut receipt, crate::daemon::now())?;
    if matches!(receipt.state.as_str(), "pending_local" | "unknown") {
        if let Err(error) = deliver_with_policy(
            paths,
            &mut ledger,
            &mut receipt,
            Some(&binding.native),
            binding.request.mode == PublicMode::Submit,
        )
        .await
        {
            if receipt.state == "pending_local"
                && matches!(error.0, "target_busy" | "generation_mismatch")
            {
                receipt.state = "rejected".into();
            }
            receipt.reason = Some(error.0.into());
            ledger.save(&receipt)?;
        }
    }
    Ok(wrap(&binding, json!(receipt)))
}

pub async fn receipt_public(paths: &Paths, id: &str, cancelling: bool) -> anyhow::Result<Value> {
    let mut ledger = Ledger::open(paths)?;
    initialize(&ledger)?;
    let Some(mut binding) = get(&ledger, id)? else {
        if super::recovery::exists(paths, id) {
            anyhow::ensure!(
                !cancelling,
                "RECOVERY_CANCELLATION_REQUIRES_VERIFIED_WORKER_STOP"
            );
            return super::recovery::pending_receipt(paths, id);
        }
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
    if cancelling {
        cancel(paths, &mut ledger, &mut receipt).await?;
    }
    let value = observe::result(paths, &mut ledger, &mut receipt).await?;
    if value["observation"]["state"] == "observed" {
        binding.evidence_observed_at = Some(crate::daemon::now());
        ledger.db.execute(
            "UPDATE public_messages SET binding=? WHERE id=?",
            params![serde_json::to_string(&binding)?, id],
        )?;
    }
    super::recovery::decorate(paths, id, wrap(&binding, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_receipt_distinguishes_native_acceptance_and_completion() {
        let b = Binding {
            cancelled: false,
            evidence_observed_at: None,
            request: PublicRequest {
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
    fn binding(input: Enqueue) -> Binding {
        Binding {
            cancelled: false,
            evidence_observed_at: None,
            request: PublicRequest {
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
        assert!(!paths.root.join("session-catalog").exists());
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
    let mut ledger = Ledger::open(paths)?;
    initialize(&ledger)?;
    let binding = if let Some(b) = get(&ledger, &request.message_id)? {
        anyhow::ensure!(b.request == request, "MESSAGE_ID_CONFLICT");
        b
    } else {
        anyhow::ensure!(
            ledger.get(&request.message_id)?.is_none(),
            "MESSAGE_ID_CONFLICT"
        );
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
