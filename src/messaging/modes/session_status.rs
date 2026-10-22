//! Timestamped, content-free observations. No launcher and no input probes.
use super::*;
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
struct Status {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "activeFlags")]
    flags: Vec<String>,
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

pub(in crate::messaging) struct Observation {
    pub state: String,
    pub reason: Option<&'static str>,
    pub previous: Option<String>,
    pub submit_supported: bool,
}
async fn read(control: &mut Transport, id: &str) -> Result<Status> {
    let read: Read = control
        .request("thread/read", json!({"threadId":id,"includeTurns":false}))
        .await
        .map_err(|e| {
            if e.0 == "timeout" {
                Fault("native_thread_read_timeout")
            } else {
                e
            }
        })?;
    if read.thread.id != id {
        return Err(Fault("correlation_changed"));
    }
    Ok(read.thread.status)
}
pub(in crate::messaging) async fn observe(
    control: &mut Transport,
    id: &str,
) -> Result<Observation> {
    let before = read(control, id).await?;
    if !before.flags.is_empty() {
        return Ok(Observation {
            state: "working".into(),
            reason: Some("approval_or_user_input_required"),
            previous: None,
            submit_supported: false,
        });
    }
    let (state, previous) = match before.kind.as_str() {
        "active" => ("working", None),
        "notLoaded" => ("disconnected", None),
        "idle" | "systemError" => {
            // Only status and identifier are deserialized; public item bodies are ignored.
            let page: Page = control
                .request(
                    "thread/turns/list",
                    json!({"threadId":id,"itemsView":"summary","sortDirection":"desc","limit":1}),
                )
                .await
                .map_err(|e| {
                    if e.0 == "timeout" {
                        Fault("native_turn_summary_timeout")
                    } else {
                        e
                    }
                })?;
            if page.data.len() > 1 {
                return Err(Fault("malformed_summary"));
            }
            match page.data.first() {
                Some(turn)
                    if turn.id.is_empty()
                        || turn.id.len() > 256
                        || turn.id.chars().any(char::is_control) =>
                {
                    return Err(Fault("malformed_summary"));
                }
                Some(turn) => (
                    match turn.status.as_str() {
                        "completed" if before.kind == "idle" => "idle",
                        "interrupted" => "interrupted",
                        "failed" => "errored",
                        _ => "unknown",
                    },
                    Some(turn.id.clone()),
                ),
                None => (
                    if before.kind == "idle" {
                        "idle"
                    } else {
                        "unknown"
                    },
                    None,
                ),
            }
        }
        _ => ("unknown", None),
    };
    if read(control, id).await? != before {
        return Err(Fault("state_changed"));
    }
    Ok(Observation {
        state: state.into(),
        reason: None,
        previous,
        submit_supported: true,
    })
}
fn actions(state: &str, reason: Option<&str>, submit_supported: bool, enabled: bool) -> Value {
    let eligible = reason.is_none() && enabled;
    let submit = eligible && (state == "working" || (state == "idle" && submit_supported));
    let queue = eligible && matches!(state, "idle" | "working");
    let resume = eligible && submit_supported && matches!(state, "interrupted" | "errored");
    let blocked = if !enabled {
        "native_sending_disabled"
    } else if let Some(reason) = reason {
        reason
    } else {
        match state {
            "idle" => "state_unverified",
            "interrupted" | "errored" => "explicit_resume_required",
            "working" => "session_already_working",
            "disconnected" => "existing_attachment_required",
            _ => "state_unverified",
        }
    };
    let resume_reason =
        if eligible && matches!(state, "interrupted" | "errored") && !submit_supported {
            "state_unverified"
        } else if eligible && state == "idle" {
            "resume_not_required"
        } else {
            blocked
        };
    json!({"submit":{"available":submit,"reason":if submit {None}else{Some(blocked)},"effective_action":if submit && state=="working" {Some("steer")}else{None},"requires_expected_turn_id":state=="working","receipt_guarantee":"acceptance_only_consumption_and_reply_unknown"},"queue":{"available":queue,"reason":if queue {None}else{Some(blocked)},"starts_eligible_idle_session":true},"resume":{"available":resume,"reason":if resume {None}else{Some(resume_reason)},"mode":"submit","requires_explicit_resume":true,"new_input_only":true,"starts_process":false}})
}
pub(in crate::messaging) async fn target(paths: &Paths, id: &str) -> Result<Value> {
    uuid(id)?;
    let ids = external::messaging_inventory(paths).map_err(|_| Fault("discovery_unavailable"))?;
    if !ids.iter().any(|known| known == id) {
        return Err(Fault("target_unavailable"));
    }
    let at = crate::daemon::now();
    let enabled = queue::sending_enabled(paths)?;
    let stop =
        external::messaging_stop_intent(paths, id).map_err(|_| Fault("discovery_unavailable"))?;
    let selected = match attachment(paths, id) {
        Ok(a) => Some(a),
        Err(Fault("unavailable")) => None,
        Err(e) => return Err(e),
    };
    let generation = selected.as_ref().map(Attachment::generation);
    let (name, account) = labels(paths, id, selected.as_ref().map(|a| a.home.as_path()))?;
    let result = async {
        let selected = selected.as_ref().ok_or(Fault("no_live_attachment"))?;
        let _guard = mutation_lock(paths, id)?;
        let mut control = selected.connect().await?;
        let observation = observe(&mut control, id).await?;
        if Some(attachment(paths, id)?.generation()) != generation {
            return Err(Fault("generation_mismatch"));
        }
        Ok(observation)
    }
    .await;
    let o = match result {
        Ok(o) => o,
        Err(e) => Observation {
            state: if matches!(
                e.0,
                "no_live_attachment" | "endpoint_unavailable" | "disconnected" | "unavailable"
            ) {
                "disconnected"
            } else {
                "unknown"
            }
            .into(),
            reason: Some(e.0),
            previous: None,
            submit_supported: false,
        },
    };
    let mut action_caps = actions(&o.state, o.reason, o.submit_supported, enabled);
    if stop.is_some() {
        for action in ["submit", "queue"] {
            action_caps[action]["available"] = json!(false);
            action_caps[action]["reason"] = json!("explicit_resume_required");
        }
        if o.state == "idle" && o.submit_supported && enabled && o.reason.is_none() {
            action_caps["resume"]["available"] = json!(true);
            action_caps["resume"]["reason"] = Value::Null;
        }
    }
    let availability = match o.state.as_str() {
        "working" => "busy",
        "idle" | "interrupted" | "errored" => "idle",
        _ => "unavailable",
    };
    Ok(
        json!({"session_id":id,"display_name":name,"account_name":account,"state":o.state,"availability":availability,"reason":o.reason,"observed_at":at,"valid_until":at+5,"generation":generation,"queued_work_count":null,"paused_or_stopped":if o.state=="interrupted" {"interrupted_explicit_resume_required"}else{"unknown_without_explicit_intent_evidence"},"check_in_eligible":false,"native_sending_enabled":enabled,"capability_id":generation.as_deref().map(queue::capability_id),"submit_capability_id":generation.as_deref().map(submit::capability_id),"previous_turn_id":o.previous,"conditional_start_supported":false,"idle_submit_supported":o.submit_supported,"idle_submit_race":"may_steer_a_concurrently_started_turn","actions":action_caps,"stop_intent_id":stop,"evidence":"native_read_only_snapshot_not_reservation"}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actions_preserve_modes_and_stop_intent() {
        for state in [
            "idle",
            "working",
            "interrupted",
            "errored",
            "disconnected",
            "unknown",
        ] {
            let old = actions(state, None, false, true);
            assert_eq!(old["resume"]["available"], false);
            assert_eq!(old["submit"]["available"], state == "working");
            let new = actions(state, None, true, true);
            if state == "idle" {
                assert!(new["submit"]["effective_action"].is_null());
            }
            assert_eq!(
                new["resume"]["available"],
                matches!(state, "interrupted" | "errored")
            );
            assert_eq!(
                new["submit"]["available"],
                matches!(state, "working" | "idle")
            );
            assert_eq!(
                new["queue"]["available"],
                matches!(state, "working" | "idle")
            );
            for reason in [None, Some("approval_or_user_input_required")] {
                let disabled = actions(state, reason, true, false);
                for action in ["submit", "queue", "resume"] {
                    assert_eq!(disabled[action]["available"], false);
                }
            }
        }
    }
}
