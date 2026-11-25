//! Native Queue uses the existing ledger, movement ownership and public reply reader.
//! Only immutable metadata is added; there is no second dispatcher or credential writer.
use super::*;

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn capability_id(generation: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{VERSION}:queue:0.163.0:{generation}").as_bytes())
    )
}
pub(in crate::messaging) fn sending_enabled(paths: &Paths) -> Result<bool> {
    match fs::symlink_metadata(paths.root.join("messaging/native-send.disabled")) {
        Ok(_) => Ok(false), // Any marker, including a symlink, disables. Never follow it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(_) => Err(Fault("native_sending_disabled")),
    }
}
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn sending(paths: &Paths, enable: bool, disable: bool) -> Result<Value> {
    // Same exclusive lock as all native enqueue calls: toggles cannot race dispatch.
    let _ledger = ledger::open_write_blocking(paths, wait_budget(paths, Wait::Send))?;
    let marker = paths.root.join("messaging/native-send.disabled");
    if disable && sending_enabled(paths)? {
        use std::os::unix::fs::OpenOptionsExt;
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)?;
        file.sync_all()?;
    } else if enable {
        match fs::remove_file(&marker) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(Fault("native_sending_disabled")),
        }
    }
    if enable || disable {
        fs::File::open(paths.root.join("messaging"))?.sync_all()?;
    }
    Ok(
        json!({"native_sending_enabled":sending_enabled(paths)?,"supported_modes":["queue","submit"],"submit_supported":"busy_only","does_not_cancel_native_queue":true}),
    )
}
#[cfg_attr(not(test), allow(dead_code))]
fn receipt(ledger: &impl Reads, id: &str) -> Result<Receipt> {
    uuid(id)?;
    if !ledger.mode_record(id)? {
        return ledger.lookup(id).and(Err(Fault("receipt_not_found")));
    }
    ledger.lookup(id)
}
#[cfg_attr(not(test), allow(dead_code))]
fn wrap(delivery: Value) -> Value {
    if !delivery["submit"].is_null() {
        return super::submit::wrap(delivery);
    }
    let state = delivery["state"].as_str().unwrap_or("unknown");
    let accepted = !delivery["native_queue_id"].is_null() || !delivery["correlation"].is_null();
    json!({"request_id":delivery["message_id"],"session_id":delivery["session_id"],"requested_mode":"queue","effective_action":if accepted {Some("queue")} else {None},"state":if state=="unknown" {"delivery_unknown"} else if state=="pending_local" {"adapter_accepted"} else {state},"facts":{"adapter_accepted":true,"native_queue_accepted":accepted,"input_recorded":!delivery["correlation"].is_null(),"input_consumed":"unknown","reply_observed":!delivery["reply"].is_null()},"delivery":delivery})
}
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn request(paths: &Paths, r: Request) -> Result<Value> {
    r.validate()?;
    let mut ledger = ledger::open_write(paths, wait_budget(paths, Wait::Send)).await?;
    let hash = r.fingerprint()?;
    if let Some(old) = ledger.get(&r.request_id)? {
        let stored: String = ledger.db.query_row(
            "SELECT hash FROM receipts WHERE id=?",
            [&r.request_id],
            |r| r.get(0),
        )?;
        if !ledger.mode_record(&r.request_id)? || stored != hash {
            return Err(Fault("idempotency_conflict"));
        }
        // A retry is a receipt lookup, never a retry of native submission.
        return Ok(wrap(json!(old)));
    }
    if r.mode == Mode::Submit {
        return super::submit::dispatch(paths, &mut ledger, r).await;
    }
    if !sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    let now = crate::daemon::now();
    if r.created_at > now + 30 || r.expires_at <= now {
        return Err(Fault("expired_or_future_request"));
    }
    if r.capability_id != capability_id(&r.expected_generation) {
        return Err(Fault("capability_changed"));
    }
    let _guard = mutation_lock(paths, &r.session_id)?;
    if external::messaging_stop_intent(paths, &r.session_id)
        .map_err(|_| Fault("discovery_unavailable"))?
        .is_some()
    {
        return Err(Fault("explicit_resume_required"));
    }
    let selected = attachment(paths, &r.session_id).map_err(|e| {
        if e.0 == "unavailable" {
            Fault("target_unavailable")
        } else {
            e
        }
    })?;
    if selected.generation() != r.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    let mut control = selected.connect().await?;
    if !matches!(
        activity(&mut control, &r.session_id).await?,
        "idle" | "busy"
    ) {
        return Err(Fault("state_unverified"));
    }
    // Lock is switcher movement ownership, not a human activity lock. Native queue
    // itself serializes conditional idle dispatch, so human starts cannot turn it into steer.
    if attachment(paths, &r.session_id)?.generation() != r.expected_generation {
        return Err(Fault("generation_mismatch"));
    }
    if !sending_enabled(paths)? {
        return Err(Fault("native_sending_disabled"));
    }
    let uncertain: i64 = ledger.db.query_row("SELECT count(*) FROM receipts r JOIN mode_requests m ON m.id=r.id WHERE json_extract(r.receipt,'$.session_id')=? AND json_extract(r.receipt,'$.state')='unknown'", [&r.session_id], |row| row.get(0))?;
    if uncertain > 0 {
        return Err(Fault("prior_delivery_unknown"));
    }
    let input = Enqueue {
        message_id: r.request_id,
        session_id: r.session_id,
        text: r.text,
        expires_at: r.expires_at,
        expected_generation: r.expected_generation,
    };
    // The mode fingerprint and uncertain dispatch intent are committed together.
    // A crash even before the socket write cannot cause an automatic retry.
    let mut receipt = ledger.accept_with_mode(&input, crate::daemon::now(), Some(&hash))?;
    submit(&mut control, &mut ledger, &mut receipt, &input).await?;
    Ok(wrap(json!(receipt)))
}
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn status(paths: &Paths, id: &str) -> Result<Value> {
    let deadline = tokio::time::Instant::now() + wait_budget(paths, Wait::Read);
    {
        // Shared first: a settled receipt needs no write and no provider read.
        let reader = ledger::open_read(paths, remaining(deadline)).await?;
        let receipt = receipt(&reader, id)?;
        if receipt.submit.is_some() {
            return Ok(wrap(json!(receipt)));
        }
        if let Some(settled) = observe::settled(&reader, &receipt, crate::daemon::now())? {
            return Ok(wrap(settled));
        }
    }
    status_write(paths, id, remaining(deadline)).await
}
#[cfg_attr(not(test), allow(dead_code))]
/// The writer path of `modes` status (may expire, re-deliver and observe).
async fn status_write(paths: &Paths, id: &str, budget: std::time::Duration) -> Result<Value> {
    let mut ledger = ledger::open_write(paths, budget).await?;
    let mut receipt = receipt(&ledger, id)?;
    if receipt.submit.is_some() {
        return Ok(wrap(json!(receipt)));
    }
    ledger.expire(&mut receipt, crate::daemon::now())?;
    if receipt.state == "unknown" {
        let _ = deliver(paths, &mut ledger, &mut receipt, None).await;
    }
    Ok(wrap(
        observe::result(paths, &mut ledger, &mut receipt).await?,
    ))
}
#[cfg(test)]
mod tests;
