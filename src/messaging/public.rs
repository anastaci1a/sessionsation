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
}

#[derive(Serialize, Deserialize)]
struct Binding {
    #[serde(default)]
    cancelled: bool,
    request: PublicRequest,
    native: Enqueue,
}

fn initialize(ledger: &Ledger) -> Result<()> {
    ledger.db.execute_batch(
        "CREATE TABLE IF NOT EXISTS public_messages(id TEXT PRIMARY KEY, binding TEXT NOT NULL);",
    )?;
    Ok(())
}

fn get(ledger: &Ledger, id: &str) -> Result<Option<Binding>> {
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
    json!({"message_id":binding.request.message_id,"session_id":binding.request.session_id,
        "accepted_generation":binding.request.expected_generation,"provider":"codex",
        "provider_conversation_id":binding.native.session_id,"mode":binding.request.mode,
        "delivery_semantics":"non_steering_native_queue",
        "state":if value["state"]=="unknown" {json!("delivery_unknown")} else {value["state"].clone()},
        "reason":value["reason"],"facts":{"accepted_local":true,"accepted_native":accepted,
            "input_recorded":!value["correlation"].is_null(),"input_consumed":"unknown",
            "reply_observed":!value["reply"].is_null()},"reply":value["reply"]})
}

pub async fn send_public(paths: &Paths, request: PublicRequest) -> anyhow::Result<Value> {
    anyhow::ensure!(
        request.api_version == crate::api::VERSION,
        "UNSUPPORTED_API_VERSION"
    );
    uuid(&request.message_id)?;
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
    let mut binding = get(&ledger, id)?.ok_or(Fault("receipt_not_found"))?;
    if binding.cancelled {
        return Ok(wrap(&binding, json!({"state":"cancelled"})));
    }
    let Some(mut receipt) = ledger.get(id)? else {
        if cancelling {
            binding.cancelled = true;
            ledger.db.execute(
                "UPDATE public_messages SET binding=? WHERE id=?",
                params![serde_json::to_string(&binding)?, id],
            )?;
            return Ok(wrap(&binding, json!({"state":"cancelled"})));
        }
        return Ok(wrap(
            &binding,
            json!({"state":"pending_local","reason":"not_dispatched"}),
        ));
    };
    if cancelling {
        cancel(paths, &mut ledger, &mut receipt).await?;
    }
    let value = observe::result(paths, &mut ledger, &mut receipt).await?;
    Ok(wrap(&binding, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_receipt_distinguishes_native_acceptance_and_completion() {
        let b = Binding {
            cancelled: false,
            request: PublicRequest {
                api_version: 1,
                message_id: uuid::Uuid::new_v4().to_string(),
                session_id: uuid::Uuid::new_v4().to_string(),
                expected_generation: "a".repeat(64),
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
