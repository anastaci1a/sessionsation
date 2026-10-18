//! Public v1 passive readiness, projected independently from legacy send modes.
use super::*;
use crate::catalog;
use std::time::Duration;

fn project(session: &catalog::Session, native: Value, removed: bool, now: i64) -> Value {
    let at = native["observed_at"].as_i64();
    let until = native["valid_until"].as_i64();
    let fresh = at
        .zip(until)
        .is_some_and(|(at, until)| at <= now && now < until);
    let state = native["state"].as_str().unwrap_or("unknown");
    let stop = native["stop_intent_id"].as_str();
    let blocked = if removed {
        Some("account_removed")
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
            "interrupted" | "errored" => Some("terminal_turn_requires_review"),
            "disconnected" => Some("existing_attachment_required"),
            _ => Some("state_unverified"),
        }
    };
    let submit = blocked.is_none() && state == "idle";
    let queue = blocked.is_none() && matches!(state, "idle" | "working");
    json!({"session_id":session.id,"generation":session.generation(),
        "provider":"codex","provider_conversation_id":session.conversation_id,
        "account_id":format!("codex:{}",session.account),"state":state,
        "reason":native["reason"],"observed_at":at,"valid_until":until,
        "freshness":if fresh {"fresh"} else {"stale"},
        "runtime_generation":native["generation"],"previous_turn_id":native["previous_turn_id"],
        "explicit_intent":{"state":if stop.is_some() {"stop_requested"} else {"unknown"},"id":stop},
        "queued_work_count":null,"evidence":"native_read_only_snapshot_not_reservation",
        "actions":{"submit":{"available":submit,"reason":if submit {None} else {blocked.or(Some("target_busy"))}},
            "queue":{"available":queue,"reason":if queue {None} else {blocked.or(Some("state_unverified"))}}},
        "delivery_semantics":"non_steering_native_queue"})
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
    let before = attachment(paths, &session.conversation_id).ok();
    if let Some(a) = &before {
        anyhow::ensure!(
            session
                .attachment
                .as_ref()
                .is_some_and(|runtime| runtime.pid == a.pid && runtime.process_start == a.start),
            "STALE_TARGET"
        );
    } else {
        anyhow::ensure!(session.attachment.is_none(), "STALE_TARGET");
    }
    let started = crate::daemon::now();
    let native = match tokio::time::timeout(
        Duration::from_secs(5),
        modes::session_status::target(paths, &session.conversation_id),
    )
    .await
    {
        Ok(Ok(value)) => value,
        result => {
            let reason = match result {
                Ok(Err(e)) => e.0,
                _ => "observation_timeout",
            };
            json!({"state":"unknown","reason":reason,"observed_at":started,"valid_until":started+5,
                "native_sending_enabled":false,"stop_intent_id":external::messaging_stop_intent(paths,&session.conversation_id)?})
        }
    };
    catalog::resolve(paths, id, generation)?;
    let after = attachment(paths, &session.conversation_id).ok();
    anyhow::ensure!(
        before.as_ref().map(Attachment::generation) == after.as_ref().map(Attachment::generation),
        "STALE_TARGET"
    );
    if let Some(a) = after {
        anyhow::ensure!(
            native["generation"].is_null() || native["generation"] == a.generation(),
            "STALE_TARGET"
        );
    }
    let mut native = native;
    let stop = external::messaging_stop_intent(paths, &session.conversation_id)?;
    if native["stop_intent_id"] != json!(stop) {
        native["state"] = json!("unknown");
        native["reason"] = json!("stop_intent_changed");
        native["stop_intent_id"] = json!(stop);
    }
    Ok(project(
        &session,
        native,
        crate::accounts::removed(paths, &session.account)?,
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
            assert!(!out.to_string().contains("SECRET"));
            assert!(!out.to_string().contains("steer\""));
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
