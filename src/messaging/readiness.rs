//! Public v1 passive readiness, projected independently from legacy send modes.
use super::*;
use crate::catalog;
use std::time::Duration;

fn observation_failure(reason: Option<&str>) -> Value {
    if reason == Some("unsupported_native_version") {
        return json!({"code":reason,"method":"initialize","message":"The account app-server version has not been verified for messaging by this Sessionsation build. Review observation.native_protocol and update Sessionsation to a compatible build; no input was sent.","retryable":false});
    }
    let (method, budget, message) = match reason {
        Some("native_thread_read_timeout") => (
            Some("thread/read"),
            3000,
            "The provider did not return thread metadata within 3 seconds. Runtime readiness remains unobserved; no input was sent.",
        ),
        Some("native_turn_summary_timeout") => (
            Some("thread/turns/list"),
            3000,
            "The provider did not return the latest turn summary within 3 seconds. Runtime readiness remains unobserved; no input was sent.",
        ),
        Some("native_initialize_timeout") => (
            Some("initialize"),
            3000,
            "The provider did not finish protocol initialization within 3 seconds. No input was sent.",
        ),
        Some("observation_timeout") => (
            None,
            5000,
            "The combined native observation exceeded its 5-second budget. No readiness or new evidence timestamp is claimed.",
        ),
        _ => return Value::Null,
    };
    json!({"code":reason,"method":method,"budget_ms":budget,"message":message,"retryable":true})
}

pub(super) fn project(session: &catalog::Session, native: Value, removed: bool, now: i64) -> Value {
    let at = native["observed_at"].as_i64();
    let until = native["valid_until"].as_i64();
    let fresh = at
        .zip(until)
        .is_some_and(|(at, until)| at <= now && now < until);
    let state = native["state"].as_str().unwrap_or("unknown");
    let stop = native["stop_intent_id"].as_str();
    let blocked = if removed {
        Some("account_removed")
    } else if native["reason"] == "session_archived" {
        Some("session_archived")
    } else if at.is_none() {
        native["reason"].as_str().or(Some("observation_required"))
    } else if !fresh {
        Some("observation_stale")
    } else if stop.is_some() {
        Some("explicit_resume_required")
    } else if let Some(reason) = native["reason"].as_str() {
        Some(reason)
    } else if native["native_sending_enabled"] != true {
        Some("native_sending_disabled")
    } else {
        match state {
            "idle" | "working" | "errored" => None,
            // An interrupted turn is recoverable, not an unspecified review dead end.
            // Use the same actionable gate as dispatch: explicitly request Resume
            // with the observed turn/runtime/stop preconditions below. A turn that
            // FAILED (for example "Selected model is at capacity") leaves a live,
            // idle session that accepts input like any other.
            "interrupted" => Some("explicit_resume_required"),
            "disconnected" => Some("existing_attachment_required"),
            _ => Some("state_unverified"),
        }
    };
    let submit = blocked.is_none() && matches!(state, "idle" | "errored");
    let queue = blocked.is_none() && matches!(state, "idle" | "working" | "errored");
    let resume = fresh
        && !removed
        && native["native_sending_enabled"] == true
        && native["reason"].is_null()
        && (matches!(state, "interrupted" | "errored") || (state == "idle" && stop.is_some()));
    let activity = {
        let facts = crate::live_activity::facts_from_readiness(
            state,
            native["reason"] == "approval_or_user_input_required",
            !native["previous_turn_id"].is_null(),
            native["last_turn"]["status"].as_str(),
            native["last_turn"]["error"].as_str(),
        );
        crate::live_activity::public(Some(&facts), at.unwrap_or(now))
    };
    json!({"session_id":session.id,"generation":session.generation(),
        "provider":session.provider,"provider_conversation_id":session.conversation_id,
        "account_id":format!("{}:{}",session.provider,session.account),"state":state,
        "reason":native["reason"],"observed_at":at,"valid_until":until,
        "freshness":if fresh {"fresh"} else {"stale"},
        "observation":{"status":if at.is_none() {"unobserved"} else if fresh {"fresh"} else {"stale"},"checked_at":now,"failure":observation_failure(native["reason"].as_str()),"turn_summary":native["summary_evidence"],"native_protocol":native["native_protocol"]},
        "runtime_state":if at.is_none() && state=="unknown" {"unobserved"} else if stop.is_some() && state!="working" {"stopped"} else {state},
        "runtime_generation":native["generation"],
        "process_state":native["process_state"].as_str().unwrap_or("unknown"),"process_observed_at":native["process_observed_at"],"previous_turn_id":native["previous_turn_id"],
        "explicit_intent":{"state":if stop.is_some() {"stop_requested"} else {"unknown"},"id":stop},
        "queued_work_count":null,"evidence":"native_read_only_snapshot_not_reservation",
        "last_turn":native["last_turn"],"activity":activity,
        "actions":{"steer":{"available":blocked.is_none() && state=="working","reason":if blocked.is_none() && state=="working" {None}else{blocked.or(Some("steer_requires_active_turn"))},"target_command":"steer-target","requires_expected_turn_id":true,"requires_expected_runtime_generation":true,"acceptance_only":false,"reply_correlation":"after_steer_item"},"submit":{"available":submit,"reason":if submit {None} else {blocked.or(Some("target_busy"))}},
            "queue":{"available":queue,"reason":if queue {None} else {blocked.or(Some("state_unverified"))}},
            "resume":{"available":resume,"reason":if resume {Value::Null} else if state=="disconnected" {json!("validated_launch_plan_required")} else if state=="idle" && stop.is_none() && fresh && native["reason"].is_null() {json!("resume_not_required")} else if state=="working" {json!("session_already_working")} else {json!(blocked.unwrap_or("state_unverified"))},"starts_process":state=="disconnected","requires_final_validation":true,"requires_expected_previous_turn_id":true,"requires_expected_stop_id":true,"requires_expected_runtime_generation":state!="disconnected","requires_launch_plan":state=="disconnected"}},
        "delivery_semantics":"non_steering_native_queue"})
}

pub(super) fn recorded(
    paths: &Paths,
    session: &catalog::Session,
) -> anyhow::Result<Option<Attachment>> {
    let list = external::messaging_recorded_bindings(paths)?;
    let selected = list
        .into_iter()
        .filter(|(id, _, _, _)| id == &session.conversation_id)
        .map(|(id, home, pid, start)| Attachment {
            id,
            home,
            pid,
            start,
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(selected.len() <= 1, "AMBIGUOUS_SESSION");
    let a = selected.into_iter().next();
    if let Some(a) = &a {
        anyhow::ensure!(
            session
                .attachment
                .as_ref()
                .is_some_and(|r| r.pid == a.pid && r.process_start == a.start),
            "STALE_TARGET"
        );
    } else if let Some(a) = &session.attachment {
        anyhow::ensure!(
            external::attachment_definitely_gone(a.pid, a.process_start),
            "ATTACHMENT_UNOBSERVED"
        );
    }
    Ok(a)
}

/// `api observe` (contract 1.63.0, SM4): the passive readiness of any provider's session plus
/// its `conversation` block (last reply, last instruction, compaction time and, with
/// `history`, the recent turns). Codex keeps its native readiness; Claude and Antigravity get
/// the same keys from their native observation; Gemini is `supported:false`.
pub async fn observe_command(
    paths: &Paths,
    id: &str,
    generation: &str,
    history: Option<usize>,
) -> anyhow::Result<Value> {
    use crate::provider::{ProviderId, conversation, native};
    let session = catalog::resolve(paths, id, generation)?;
    let history = history.map(|n| n.clamp(1, conversation::MAX_TURNS));
    let Some(provider) = native::of(&session.provider) else {
        let mut value = observe_public(paths, id, generation).await?;
        let home = crate::layout::account_home(&paths.root, ProviderId::Codex, &session.account);
        let file = conversation::history_file(ProviderId::Codex, &home, &session.conversation_id);
        value["conversation"] = conversation::observe(ProviderId::Codex, file.as_deref(), history);
        return Ok(value);
    };
    let now = crate::daemon::now();
    if conversation::supported(provider).is_err() {
        let mut value = project(
            &session,
            json!({"state":"unknown","reason":"provider_unsupported","observed_at":null,"valid_until":null,
                "native_sending_enabled":false,"process_state":"unknown"}),
            false,
            now,
        );
        value["delivery_semantics"] = Value::Null;
        value["conversation"] = conversation::observe(provider, None, history);
        return Ok(value);
    }
    let (session, o) = crate::api::native_selected(paths, id, generation)?;
    let account = native::account(paths, provider, &session.account)?;
    let archived = crate::external::archiving::is_archived(paths, &session.id)?;
    let running = o.availability == "running";
    let mut row = json!({});
    crate::native_activity::apply(provider, &account.home, &mut row, Some(&o), now);
    let live = &row["live_activity"];
    let state = match (archived, running, live["state"].as_str()) {
        (true, _, _) => "disconnected",
        (_, false, _) => "disconnected",
        (_, true, Some("working" | "blocked")) => "working",
        (_, true, Some("idle" | "turn_ended")) => "idle",
        (_, true, Some("error")) => "errored",
        _ => "unknown",
    };
    // Native input is gated by `api send` (terminal typing or headless), not by this read.
    let reason = if archived {
        "session_archived"
    } else if running {
        "native_input_via_send"
    } else {
        "no_live_process"
    };
    let process_state = if running {
        "running"
    } else if session.attachment.is_some() {
        "exited"
    } else {
        "unattached"
    };
    let mut value = project(
        &session,
        json!({"state":state,"reason":reason,"observed_at":now,"valid_until":now+5,
            "last_turn":{"status":live["last_turn_status"],"error":live["error"]},
            "native_sending_enabled":false,"stop_intent_id":null,
            "generation":null,
            "process_state":process_state,"process_observed_at":now}),
        crate::accounts::removed_session(paths, &session.provider, &session.account)?,
        now,
    );
    value["delivery_semantics"] = json!("api_send");
    let file = conversation::history_file(provider, &account.home, &session.conversation_id);
    value["conversation"] = conversation::observe(provider, file.as_deref(), history);
    Ok(value)
}

pub async fn observe_public(paths: &Paths, id: &str, generation: &str) -> anyhow::Result<Value> {
    let session = catalog::resolve(paths, id, generation)?;
    crate::provider::ops::require_bare(crate::provider::ops::supervised(&session.provider))?;
    anyhow::ensure!(
        !matches!(session.account.as_str(), "external" | "unknown"),
        "ACCOUNT_UNAVAILABLE"
    );
    if crate::external::archiving::metadata(paths)?
        .get(id)
        .is_some_and(|v| v["archived"] == true)
    {
        return Ok(project(
            &session,
            json!({"state":"disconnected","reason":"session_archived"}),
            false,
            crate::daemon::now(),
        ));
    }
    let before = recorded(paths, &session)?;
    let started = crate::daemon::now();
    let mut provider_version = None;
    let result = async {
        let a = before.as_ref().ok_or(Fault("no_live_attachment"))?;
        let mut control = a
            .connect_with_profile(&mut provider_version)
            .await
            .map_err(|e| {
                if e.0 == "timeout" {
                    Fault("native_initialize_timeout")
                } else {
                    e
                }
            })?;
        modes::session_status::observe_cached(&mut control, paths, a).await
    };
    let mut native = match tokio::time::timeout(Duration::from_secs(5), result).await {
        Ok(Ok(o)) => {
            // An unchanged rollout cannot prove that an in-memory turn has not
            // started. Preserve the age of the native summary; status reads
            // and filesystem checks must never renew that evidence.
            let at = o
                .summary_evidence
                .as_ref()
                .and_then(|v| v["observed_at"].as_i64())
                .unwrap_or_else(crate::daemon::now);
            json!({"state":o.state,"reason":o.reason,"previous_turn_id":o.previous,
                "last_turn":{"status":o.last_turn_status,"error":o.last_turn_error},
                "summary_evidence":o.summary_evidence,
                "observed_at":at,"valid_until":at+5,"observation_started_at":started,
                "native_sending_enabled":modes::queue::sending_enabled(paths)?,
                "stop_intent_id":external::messaging_stop_intent(paths,&session.conversation_id)?,
                "generation":before.as_ref().map(Attachment::generation)})
        }
        result => json!({"state":if before.is_none() {"disconnected"} else {"unknown"},
            "reason":match result {Ok(Err(e))=>e.0,_=>"observation_timeout"},
            "observed_at":null,"valid_until":null,"native_sending_enabled":false}),
    };
    native["native_protocol"] = json!({"version":provider_version,"supported_versions":NATIVE_PROFILES,"supported":provider_version.as_deref().map(|v|NATIVE_PROFILES.contains(&v))});
    catalog::resolve(paths, id, generation)?;
    let after = recorded(paths, &session)?;
    anyhow::ensure!(
        before.as_ref().map(Attachment::generation) == after.as_ref().map(Attachment::generation),
        "STALE_TARGET"
    );
    let stop = external::messaging_stop_intent(paths, &session.conversation_id)?;
    if native["observed_at"].is_number() && native["stop_intent_id"] != json!(stop) {
        native["state"] = json!("unknown");
        native["reason"] = json!("stop_intent_changed");
        native["stop_intent_id"] = json!(stop);
    }
    native["stop_intent_id"] = json!(stop);
    // Runtime identity is independent of successful native state observation.
    native["generation"] = json!(before.as_ref().map(Attachment::generation));
    native["process_state"] = json!(if before.is_some() {
        "running"
    } else if session.attachment.is_some() {
        "exited"
    } else {
        "unattached"
    });
    native["process_observed_at"] = json!(crate::daemon::now());
    if crate::lifecycle::pending(paths, &session.conversation_id, None)?.is_some() {
        native["reason"] = json!("session_operation_in_progress");
    }
    Ok(project(
        &session,
        native,
        crate::accounts::removed_session(paths, &session.provider, &session.account)?,
        crate::daemon::now(),
    ))
}

#[cfg(test)]
mod tests;
