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
            "idle" | "working" => None,
            // A terminal turn is recoverable, not an unspecified review dead end.
            // Use the same actionable gate as dispatch: explicitly request Resume
            // with the observed turn/runtime/stop preconditions below.
            "interrupted" | "errored" => Some("explicit_resume_required"),
            "disconnected" => Some("existing_attachment_required"),
            _ => Some("state_unverified"),
        }
    };
    let submit = blocked.is_none() && state == "idle";
    let queue = blocked.is_none() && matches!(state, "idle" | "working");
    let resume = fresh
        && !removed
        && native["native_sending_enabled"] == true
        && native["reason"].is_null()
        && (matches!(state, "interrupted" | "errored") || (state == "idle" && stop.is_some()));
    json!({"session_id":session.id,"generation":session.generation(),
        "provider":"codex","provider_conversation_id":session.conversation_id,
        "account_id":format!("codex:{}",session.account),"state":state,
        "reason":native["reason"],"observed_at":at,"valid_until":until,
        "freshness":if fresh {"fresh"} else {"stale"},
        "observation":{"status":if at.is_none() {"unobserved"} else if fresh {"fresh"} else {"stale"},"checked_at":now,"failure":observation_failure(native["reason"].as_str()),"turn_summary":native["summary_evidence"],"native_protocol":native["native_protocol"]},
        "runtime_state":if at.is_none() && state=="unknown" {"unobserved"} else if stop.is_some() && state!="working" {"stopped"} else {state},
        "runtime_generation":native["generation"],
        "process_state":native["process_state"].as_str().unwrap_or("unknown"),"process_observed_at":native["process_observed_at"],"previous_turn_id":native["previous_turn_id"],
        "explicit_intent":{"state":if stop.is_some() {"stop_requested"} else {"unknown"},"id":stop},
        "queued_work_count":null,"evidence":"native_read_only_snapshot_not_reservation",
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

pub async fn observe_public(paths: &Paths, id: &str, generation: &str) -> anyhow::Result<Value> {
    let session = catalog::resolve(paths, id, generation)?;
    anyhow::ensure!(
        session.provider == "codex",
        "UNSUPPORTED_PROVIDER_OPERATION"
    );
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
mod tests {
    use super::*;
    fn session() -> catalog::Session {
        catalog::Session {
            id: uuid::Uuid::new_v4().to_string(),
            name: "test".into(),
            provider: "codex".into(),
            account: "selected".into(),
            conversation_id: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            attachment: None,
            directory: None,
            previous_conversation_ids: vec![],
        }
    }
    #[test]
    fn passive_hints_never_adopt_legacy_busy_steer_or_infer_stop_intent() {
        let s = session();
        for state in [
            "idle",
            "working",
            "interrupted",
            "errored",
            "disconnected",
            "unknown",
        ] {
            let native = json!({"state":state,"observed_at":100,"valid_until":105,"native_sending_enabled":true,"actions":{"submit":{"effective_action":"steer"}},"private":"SECRET"});
            let out = project(&s, native, false, 101);
            assert_eq!(out["actions"]["submit"]["available"], state == "idle");
            assert_eq!(
                out["actions"]["queue"]["available"],
                matches!(state, "idle" | "working")
            );
            assert_eq!(out["explicit_intent"]["state"], "unknown");
            assert_eq!(
                out["actions"]["resume"]["requires_launch_plan"],
                state == "disconnected"
            );
            assert_eq!(
                out["actions"]["resume"]["requires_expected_runtime_generation"],
                state != "disconnected"
            );
            assert!(!out.to_string().contains("SECRET"));
            assert!(out["actions"]["submit"]["effective_action"].is_null());
            assert_eq!(out["actions"]["steer"]["available"], state == "working");
        }
    }
    #[test]
    fn stale_disabled_removed_and_explicit_stop_block_all_readiness() {
        for (removed, now, enabled, stop) in [
            (false, 105, true, None),
            (false, 99, true, None),
            (true, 101, true, None),
            (false, 101, false, None),
            (false, 101, true, Some("stop-token")),
        ] {
            let out = project(
                &session(),
                json!({"state":"idle","observed_at":100,"valid_until":105,"native_sending_enabled":enabled,"stop_intent_id":stop}),
                removed,
                now,
            );
            assert_eq!(out["actions"]["submit"]["available"], false);
            assert_eq!(out["actions"]["queue"]["available"], false);
            assert!(out["actions"]["queue"]["reason"].is_string());
        }
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use crate::messaging::tests::{attachment_fixture, bind, fixture, scripted_server};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_native_reads_report_the_exact_rpc_without_claiming_evidence() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        for method in ["thread/read", "thread/turns/list"] {
            let (_dir, paths, input, _) = fixture();
            let mut a = attachment_fixture(&paths, &input);
            bind(&paths, &mut a);
            let i: external::Instance = serde_json::from_value(json!({"pane":"%1","tmux_session":"test","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
            let s = catalog::reconcile(&paths, &[i]).unwrap().remove(0);
            let listener = tokio::net::UnixListener::bind(a.endpoint()).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(Message::Text(raw))) = ws.next().await {
                    let req: Value = serde_json::from_str(&raw).unwrap();
                    let name = req["method"].as_str().unwrap();
                    let result = match name {
                        "initialized" => continue,
                        "initialize" => json!({"userAgent":"fixture/0.163.0 (Linux)"}),
                        "thread/read" => {
                            json!({"thread":{"id":req["params"]["threadId"],"status":{"type":"idle"}}})
                        }
                        "thread/turns/list" => json!({"data":[{"id":"last","status":"completed"}]}),
                        _ => panic!("Unexpected mutating request {name}"),
                    };
                    if name == method {
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    }
                    if ws
                        .send(Message::Text(
                            json!({"id":req["id"],"result":result}).to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let out = observe_public(&paths, &s.id, &s.generation())
                .await
                .unwrap();
            server.abort();
            assert_eq!(out["observation"]["failure"]["method"], method);
            assert_eq!(out["observation"]["failure"]["budget_ms"], 3000);
            assert_eq!(out["runtime_state"], "unobserved");
            assert_eq!(out["observation"]["status"], "unobserved");
            assert!(out["observed_at"].is_null() && out["valid_until"].is_null());
            for action in ["submit", "queue", "resume"] {
                assert_eq!(out["actions"][action]["available"], false);
            }
        }
    }

    #[tokio::test]
    async fn passive_observation_uses_only_read_only_rpc_and_rejects_catalog_races() {
        for (busy, rename) in [(false, false), (true, false), (false, true)] {
            let (_dir, paths, input, _) = fixture();
            let mut a = attachment_fixture(&paths, &input);
            bind(&paths, &mut a);
            let i:external::Instance=serde_json::from_value(json!({"pane":"%1","tmux_session":"test","pid":a.pid,"start_time":a.start,"cwd":"/tmp","thread_id":a.id,"rollout":"/tmp/unused","home":a.home,"binary":"fixture","resume_options":[],"account":"selected"})).unwrap();
            let s = catalog::reconcile(&paths, &[i]).unwrap().remove(0);
            let changed = Arc::new(AtomicBool::new(false));
            let flag = changed.clone();
            let cp = paths.clone();
            let cs = s.clone();
            let server=scripted_server(&a.endpoint(),move |req|{
                match req["method"].as_str().unwrap() {
                    "thread/read"=>{
                        assert_eq!(req["params"]["includeTurns"],false);
                        if rename && !flag.swap(true,Ordering::SeqCst) {catalog::rename(&cp,&cs.id,&cs.generation(),"changed").unwrap();}
                        Some(json!({"thread":{"id":req["params"]["threadId"],"status":{"type":if busy {"active"} else {"idle"}},"history":"SECRET"}}))
                    },
                    "thread/turns/list"=>{assert_eq!(req["params"]["itemsView"],"summary");assert_eq!(req["params"]["limit"],1);Some(json!({"data":[{"id":"turn-1","status":"completed","private":"SECRET"}]}))},
                    other=>panic!("Readiness must never call {other}"),
                }
            }).await;
            let out = observe_public(&paths, &s.id, &s.generation()).await;
            server.abort();
            if rename {
                assert!(out.unwrap_err().to_string().contains("STALE_TARGET"));
            } else {
                let out = out.unwrap();
                assert_eq!(out["state"], if busy { "working" } else { "idle" });
                assert_eq!(out["actions"]["submit"]["available"], !busy);
                assert_eq!(out["actions"]["queue"]["available"], true);
                assert!(!out.to_string().contains("SECRET"));
            }
        }
    }
}
